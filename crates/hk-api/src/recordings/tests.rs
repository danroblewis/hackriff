//! T-469: the route's validation, and the wire shape of the catalogue it serves.

use std::sync::Arc;

use hk_model::provenance::{ClockSource, Provenance, Tune};
use hk_model::recording::{Recording, RecordingKind, RecordingTrigger};
use hk_model::time::TimestampMethod;
use hk_model::{
    BiasTee, ContentClass, ProvenanceId, RecordingId, RetentionClass, TimeRange, Timestamp,
};
use hk_store::recordings::{Availability, AvailableSpan, RecordingEntry, RecordingsCatalogue};

use super::*;

/// A catalogue that answers with whatever it was built with, remembering the query it was asked.
struct Fake {
    catalogue: RecordingsCatalogue,
    seen: std::sync::Mutex<Vec<RecordingsQuery>>,
}

impl Fake {
    fn new(catalogue: RecordingsCatalogue) -> Arc<Self> {
        Arc::new(Self {
            catalogue,
            seen: std::sync::Mutex::new(Vec::new()),
        })
    }
}

impl RecordingCatalog for Fake {
    fn list(&self, query: &RecordingsQuery) -> Result<RecordingsCatalogue, RecordingsFailure> {
        self.seen.lock().unwrap().push(*query);
        Ok(self.catalogue.clone())
    }
}

fn prov() -> Provenance {
    Provenance {
        device_id: "hackrf:deadbeef".into(),
        tune: Tune {
            center_hz: 101.3e6,
            sample_rate_hz: 2.4e6,
            lna_db: 16.0,
            vga_db: 20.0,
            amp_on: false,
            bandwidth_hz: 1.75e6,
        },
        overload: false,
        quantisation_limited: false,
        noise_sigma_lsb: None,
        temperature_c: None,
        antenna_port: Some("ANT".into()),
        bias_tee: BiasTee::Off,
        clock_source: ClockSource::Internal,
        clock_locked: true,
        calibration_state_ref: None,
        spur_mask_ref: None,
        timestamp_method: TimestampMethod::Synthetic,
        timestamp_error_budget_ns: None,
        capture_artefacts: Vec::new(),
    }
}

fn recording(id: RecordingId, t0_ns: i64, t1_ns: i64) -> Recording {
    Recording {
        id,
        meta_uri: format!("recordings/{id}.sigmf-meta"),
        data_uri: format!("recordings/{id}.sigmf-data"),
        kind: RecordingKind::IqSnippet,
        time: TimeRange::new(
            Timestamp::from_unix_nanos(t0_ns),
            Timestamp::from_unix_nanos(t1_ns),
        ),
        f_center_hz: 101.3e6,
        sample_rate_hz: 2.4e6,
        trigger: RecordingTrigger::Manual,
        pre_trigger_s: 0.0,
        post_trigger_s: (t1_ns - t0_ns) as f64 / 1e9,
        size_bytes: 4800,
        retention_class: RetentionClass::Pinned,
        content_class: ContentClass::Unrestricted,
        provenance_ref: ProvenanceId::new(),
    }
}

fn entry(r: Recording, availability: Availability, bytes_on_disk: Option<u64>) -> RecordingEntry {
    RecordingEntry {
        recording: r,
        provenance: Some(prov()),
        availability,
        bytes_on_disk,
        meta_present: true,
        detail: (availability != Availability::Complete).then(|| "short".to_owned()),
    }
}

fn get(state: &ApiState, query: &[(&str, &str)]) -> (u16, Value) {
    let q: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let req = CtlRequest {
        method: "GET",
        path: "/api/recordings",
        body: b"",
        content_type: None,
        caller: crate::control::Caller::default(),
        query: &q,
    };
    let r = route(state, &req).expect("mine");
    (r.status, r.body)
}

/// The page, its per-recording extent, tuning and provenance, and the honest availability of each.
#[test]
fn the_route_serves_extent_tuning_provenance_and_what_is_actually_on_disk() {
    let (whole, half) = (RecordingId::new(), RecordingId::new());
    let catalogue = RecordingsCatalogue {
        entries: vec![
            entry(
                recording(half, 5_000_000_000, 6_000_000_000),
                Availability::Partial,
                Some(1200),
            ),
            entry(
                recording(whole, 1_000_000_000, 3_000_000_000),
                Availability::Complete,
                Some(4800),
            ),
        ],
        matched: 3,
        omitted: 1,
        spans: vec![AvailableSpan {
            t0_ns: 1_000_000_000,
            t1_ns: 3_000_000_000,
            recording: whole,
        }],
    };
    let state = ApiState {
        recordings: Some(Fake::new(catalogue)),
        ..ApiState::default()
    };

    let (st, v) = get(&state, &[]);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["count"], json!(2), "{v}");
    assert_eq!(v["matched"], json!(3), "{v}");
    assert_eq!(v["omitted"], json!(1), "{v}");

    let r = &v["recordings"][1];
    assert_eq!(r["id"], json!(whole.to_string()), "{r}");
    assert_eq!(r["kind"], json!("iq-snippet"), "{r}");
    assert_eq!(r["iq"], json!(true), "{r}");
    assert_eq!(r["t0"], json!(1.0), "{r}");
    assert_eq!(r["t1"], json!(3.0), "{r}");
    assert_eq!(r["t0_ns"], json!(1_000_000_000i64), "{r}");
    assert_eq!(r["t1_ns"], json!(3_000_000_000i64), "{r}");
    assert_eq!(r["duration_s"], json!(2.0), "{r}");
    assert_eq!(r["center_hz"], json!(101.3e6), "{r}");
    assert_eq!(r["sample_rate_hz"], json!(2.4e6), "{r}");
    // centre ± rate/2, so a client can place the recording on the frequency axis.
    assert_eq!(r["f_lo"], json!(101.3e6 - 1.2e6), "{r}");
    assert_eq!(r["f_hi"], json!(101.3e6 + 1.2e6), "{r}");
    // Where the pipeline reads it from.
    assert_eq!(
        r["data_uri"],
        json!(format!("recordings/{whole}.sigmf-data")),
        "{r}"
    );
    assert_eq!(r["size_bytes"], json!(4800), "{r}");
    // Which front end captured it.
    assert_eq!(r["device_id"], json!("hackrf:deadbeef"), "{r}");
    assert_eq!(r["antenna_port"], json!("ANT"), "{r}");
    assert_eq!(r["bias_tee"], json!("off"), "{r}");
    assert_eq!(r["bandwidth_hz"], json!(1.75e6), "{r}");
    assert_eq!(r["lna_db"], json!(16.0), "{r}");
    assert_eq!(r["overload"], json!(false), "{r}");
    assert_eq!(r["trigger"], json!({"kind": "manual"}), "{r}");
    assert_eq!(r["retention_class"], json!("pinned"), "{r}");
    // On disk, now.
    assert_eq!(r["state"], json!("complete"), "{r}");
    assert_eq!(r["available"], json!(true), "{r}");
    assert_eq!(r["bytes_on_disk"], json!(4800), "{r}");
    assert!(r["detail"].is_null(), "{r}");

    // The partially written one is listed, and is not available.
    let p = &v["recordings"][0];
    assert_eq!(p["id"], json!(half.to_string()), "{p}");
    assert_eq!(p["state"], json!("partial"), "{p}");
    assert_eq!(p["available"], json!(false), "{p}");
    assert_eq!(p["bytes_on_disk"], json!(1200), "{p}");
    assert_eq!(p["detail"], json!("short"), "{p}");

    // ...and contributes no span: only complete IQ extends the audio horizon.
    let iq = &v["iq_available"];
    assert_eq!(iq["horizon"], json!("iq-ring + recordings"), "{iq}");
    assert_eq!(iq["spans"].as_array().unwrap().len(), 1, "{iq}");
    let s = &iq["spans"][0];
    assert_eq!(s["source"], json!("recording"), "{s}");
    assert_eq!(s["recording"], json!(whole.to_string()), "{s}");
    assert_eq!(s["t0"], json!(1.0), "{s}");
    assert_eq!(s["t1"], json!(3.0), "{s}");
    assert_eq!(s["t0_ns"], json!(1_000_000_000i64), "{s}");
    assert_eq!(s["span_s"], json!(2.0), "{s}");
    // No ring on this server: said so, not silently dropped.
    assert_eq!(iq["ring"]["enabled"], json!(false), "{iq}");
    assert_eq!(
        iq["ring"]["reason"],
        json!("no IQ capture buffer on this server"),
        "{iq}"
    );
}

/// The ring's window joins the recordings' spans in one list, each naming its source, sorted on
/// the one shared time axis — and is never merged with them into a single envelope.
#[test]
fn the_ring_window_joins_the_recording_spans_as_its_own_labelled_span() {
    struct Ring;
    impl crate::iqbuffer::IqBufferControl for Ring {
        fn status(&self, _q: &crate::iqbuffer::IqBufferQuery) -> Value {
            json!({"enabled": true, "reason": Value::Null, "t0": 100.0, "t1": 160.0})
        }
        fn clip(
            &self,
            _r: &crate::iqbuffer::ClipStart,
        ) -> Result<Value, crate::iqbuffer::IqBufferFailure> {
            unreachable!("not a clip test")
        }
    }
    let old = RecordingId::new();
    let state = ApiState {
        recordings: Some(Fake::new(RecordingsCatalogue {
            entries: vec![entry(
                recording(old, 10_000_000_000, 20_000_000_000),
                Availability::Complete,
                Some(4800),
            )],
            matched: 1,
            omitted: 0,
            spans: vec![AvailableSpan {
                t0_ns: 10_000_000_000,
                t1_ns: 20_000_000_000,
                recording: old,
            }],
        })),
        iq_buffer: Some(Arc::new(Ring)),
        ..ApiState::default()
    };

    let (st, v) = get(&state, &[]);
    assert_eq!(st, 200, "{v}");
    let iq = &v["iq_available"];
    assert_eq!(iq["ring"]["enabled"], json!(true), "{iq}");
    assert_eq!(iq["ring"]["t0"], json!(100.0), "{iq}");
    assert_eq!(iq["ring"]["t1"], json!(160.0), "{iq}");
    let spans = iq["spans"].as_array().unwrap();
    assert_eq!(spans.len(), 2, "{iq}");
    // Oldest first: the recording (10..20 s), then the ring (100..160 s). The gap between them
    // is real, and stays visible because nothing merged them.
    assert_eq!(spans[0]["source"], json!("recording"), "{iq}");
    assert_eq!(spans[0]["t1"], json!(20.0), "{iq}");
    assert_eq!(spans[1]["source"], json!("ring"), "{iq}");
    assert_eq!(spans[1]["t0"], json!(100.0), "{iq}");
    assert_eq!(spans[1]["t1"], json!(160.0), "{iq}");
    assert!(spans[1]["recording"].is_null(), "{iq}");
    assert!(v["iq_available"].get("t0").is_none(), "no envelope: {iq}");
}

/// Query parameters reach the store as the store's own units, and bad ones are refused.
#[test]
fn the_query_is_validated_and_converted_to_the_models_units() {
    let fake = Fake::new(RecordingsCatalogue {
        entries: Vec::new(),
        matched: 0,
        omitted: 0,
        spans: Vec::new(),
    });
    let state = ApiState {
        recordings: Some(Arc::clone(&fake) as Arc<dyn RecordingCatalog>),
        ..ApiState::default()
    };

    let (st, v) = get(
        &state,
        &[
            ("t0", "1.5"),
            ("t1", "2.25"),
            ("kind", "audio"),
            ("limit", "7"),
        ],
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["count"], json!(0), "{v}");
    assert!(
        v["iq_available"]["spans"].as_array().unwrap().is_empty(),
        "{v}"
    );
    let seen = fake.seen.lock().unwrap();
    assert_eq!(
        seen[0],
        RecordingsQuery {
            // Unix seconds on the wire, integer ns to the model: converted once, here.
            t0_ns: Some(1_500_000_000),
            t1_ns: Some(2_250_000_000),
            kind: Some(RecordingKind::Audio),
            limit: 7,
        }
    );
    drop(seen);

    // The default page size, when none is named.
    let (st, _) = get(&state, &[]);
    assert_eq!(st, 200);
    assert_eq!(
        fake.seen.lock().unwrap().last().unwrap().limit,
        DEFAULT_RECORDINGS_PAGE
    );

    for bad in [
        vec![("bogus", "1")],
        vec![("limit", "0")],
        vec![("limit", "1001")],
        vec![("limit", "x")],
        vec![("t0", "abc")],
        vec![("t0", "-5")],
        vec![("t0", "5"), ("t1", "4")],
        vec![("kind", "video")],
        vec![("kind", "iq")],
    ] {
        let (st, v) = get(&state, &bad);
        assert_eq!((st, v["code"].as_str()), (400, Some("invalid")), "{bad:?}");
    }
}

/// Without a catalogue the route says so, and never pretends there are no recordings.
#[test]
fn no_catalogue_is_503_unavailable_not_an_empty_list() {
    let (st, v) = get(&ApiState::default(), &[]);
    assert_eq!((st, v["code"].as_str()), (503, Some("unavailable")), "{v}");
    assert!(v["recordings"].is_null(), "{v}");
}

#[test]
fn recordings_route_resolves() {
    assert!(matches!(resolve("GET", "/api/recordings"), Some(Ok(()))));
    assert!(matches!(
        resolve("POST", "/api/recordings"),
        Some(Err(Some("GET")))
    ));
    assert!(resolve("GET", "/api/recordings/x").is_none());
}
