//! T-050 end to end over HTTP: the composition `hk serve` uses (`serve_api` + the pipeline
//! controller adapters) over a scripted retunable radio behind the device contract. Display,
//! pause, manual recording, in-place retune, a retune into another content class (re-plumb),
//! a rate change (re-plumb),
//! named gains, bias tee, audit and bookmarks; and a replayed recording refusing device settings.

#[path = "../../hk-pipeline/tests/support/radio.rs"]
mod radio;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hk_api::{LiveControl, LiveTuning, SourceLiveControl, StreamRegistry, Token};
use hk_cli::control::PipelineRetuner;
use hk_cli::pipeline::{TempDataDirGuard, serve_api, temp_data_dir};
use hk_cli::serve::{ServeOptions, ServeSource, Serving, start};
use hk_core::Source;
use hk_model::{Repository, Timestamp};
use hk_pipeline::class::window_class;
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, replay_plan};
use serde_json::{Value, json};

const TOKEN: &str = "t050-http-control-token-0123456789ab";
const FM: f64 = 100.8e6;
const FS: f64 = 250e3;

fn call(addr: SocketAddr, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(90))).unwrap();
    let body = body.unwrap_or("");
    let ct = if body.is_empty() {
        String::new()
    } else {
        format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        )
    };
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TOKEN}\r\n{ct}\
         Connection: close\r\n\r\n{body}"
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let status = raw[9..12].parse().unwrap();
    let body = raw.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

fn post(addr: SocketAddr, path: &str, body: &str) -> (u16, Value) {
    call(addr, "POST", path, Some(body))
}

fn wait_for(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn spectrum_class(addr: SocketAddr) -> Option<String> {
    let (_, v) = call(addr, "GET", "/api/streams", None);
    v["streams"]
        .as_array()?
        .iter()
        .find(|s| s["stream_id"] == "spectrum/live")
        .and_then(|s| s["content_class"].as_str().map(str::to_owned))
}

#[test]
fn the_control_api_drives_a_live_run_through_class_changes_and_rate_changes() {
    let dir = temp_data_dir();
    let _guard = TempDataDirGuard::new(dir.clone());
    let (radio, ctl) = radio::Radio::new(FM, FS, 4096, radio::tone(|_| 50e3));
    let control = radio.control();
    let registry = StreamRegistry::new();
    let mut cfg = PipelineConfig::new(
        &dir,
        replay_plan(FM, FS, Timestamp::from_unix_nanos(radio::T0_NS)),
    )
    .unwrap();
    let reg = registry.clone();
    cfg.stream_sink = Some(Arc::new(move |h, p| reg.register(h, p)));
    cfg.source_class = window_class(FM, FS);
    cfg.live_window_class = true;
    cfg.lossless = true;
    cfg.settings.chains = Some(Vec::new());
    let handle = Pipeline::start(
        cfg,
        Box::new(radio),
        SourceInfo {
            sample_rate_hz: FS,
            center_hz: FM,
            start_time: Timestamp::from_unix_nanos(radio::T0_NS),
        },
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let live = Arc::new(
        SourceLiveControl::new(
            control,
            LiveTuning {
                center_hz: FM,
                sample_rate_hz: FS,
                gains: Vec::new(),
                bias_tee: Some(false),
                baseband_filter_hz: None,
            },
        )
        .with_retuner(Arc::new(PipelineRetuner(handle.controller()))),
    ) as Arc<dyn LiveControl>;
    let token = Token::from_config(TOKEN).unwrap();
    let token_id = token.id();
    let server = serve_api(
        "127.0.0.1:0".parse().unwrap(),
        None,
        &registry,
        &handle,
        token,
        "t050",
        Some(live),
    )
    .unwrap();
    let addr = server.local_addr();

    let (st, state) = call(addr, "GET", "/api/control/state", None);
    assert_eq!(st, 200, "{state}");
    assert_eq!(state["live"], json!(true));
    assert_eq!(state["run"]["content_class"], "unrestricted");
    assert_eq!(state["transmit"]["available"], json!(false));
    wait_for("the spectrum stream", Duration::from_secs(60), || {
        spectrum_class(addr).as_deref() == Some("unrestricted")
    });

    // Display settings apply live.
    let (st, v) = post(
        addr,
        "/api/control/display",
        r#"{"fft_size": 512, "averaging": 4, "rows_per_s": 10}"#,
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        v["display"],
        json!({"fft_size": 512, "averaging": 4, "rows_per_s": 10.0, "paused": false, "window": "hann"})
    );
    let (st, v) = post(addr, "/api/control/display", r#"{"fft_size": 1000}"#);
    assert_eq!((st, v["code"].as_str()), (400, Some("invalid")));
    assert_eq!(
        post(addr, "/api/control/pause", "{}").1["display"]["paused"],
        json!(true)
    );
    assert_eq!(
        post(addr, "/api/control/resume", "{}").1["display"]["paused"],
        json!(false)
    );

    // A manual recording under broadcast FM (content permitted) is stored.
    let (st, v) = post(
        addr,
        "/api/control/record/start",
        r#"{"label": "fm tone", "max_s": 0.2}"#,
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["recording"]["active"], json!(true));
    wait_for("the recording to end", Duration::from_secs(120), || {
        !call(addr, "GET", "/api/control/state", None).1["run"]["recording"]["active"]
            .as_bool()
            .unwrap_or(true)
    });
    let (_, state) = call(addr, "GET", "/api/control/state", None);
    let rec = &state["run"]["recording"];
    assert_eq!(rec["stored"], json!(true), "{rec}");
    assert_eq!(rec["samples"], json!(50_000));
    assert_eq!(rec["ended"], "maximum duration reached");

    // In the same class: tuned in place.
    let (st, v) = post(addr, "/api/control/center", r#"{"center_hz": 101.0e6}"#);
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        (
            v["run"]["segment"].as_u64(),
            v["run"]["content_class"].as_str()
        ),
        (Some(0), Some("unrestricted"))
    );

    // Into 930.5 MHz (another content class): the run re-plumbs.
    let (st, v) = post(addr, "/api/control/center", r#"{"center_hz": 930.5e6}"#);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["tuning"]["center_hz"], json!(930.5e6));
    assert_eq!(v["run"]["segment"], json!(1));

    // A rate change re-plumbs too.
    let (st, v) = post(addr, "/api/control/rate", r#"{"sample_rate_hz": 500e3}"#);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["tuning"]["sample_rate_hz"], json!(500e3));
    assert_eq!(v["run"]["segment"], json!(2));
    let (st, v) = post(addr, "/api/control/rate", r#"{"sample_rate_hz": 40e6}"#);
    assert_eq!((st, v["code"].as_str()), (400, Some("out_of_range")));

    // Named gains and bias tee go to the device.
    let (st, v) = post(
        addr,
        "/api/control/gains",
        r#"{"gains": {"lna": 30, "vga": 21}}"#,
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["tuning"]["gains"], json!({"lna": 24.0, "vga": 20.0}));
    assert_eq!(
        post(addr, "/api/control/bias_tee", r#"{"enabled": true}"#).0,
        200
    );
    wait_for(
        "the device to deliver the new window",
        Duration::from_secs(60),
        || {
            ctl.windows
                .lock()
                .unwrap()
                .iter()
                .any(|w| w.1 == 930.5e6 && w.2 == 500e3)
        },
    );
    let calls = ctl.calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec![
            "tune 101000000".to_string(),
            "tune 930500000".to_string(),
            "rate 500000".to_string(),
            "gain lna 24".to_string(),
            "gain vga 20".to_string(),
            "bias true".to_string()
        ]
    );
    let (_, status) = call(addr, "GET", "/api/status", None);
    assert_eq!(
        status["control"]["stats"]["replumbs"],
        json!(2),
        "{}",
        status["control"]
    );

    // A bookmark.
    let (st, bm) = post(
        addr,
        "/api/bookmarks",
        r#"{"name": "paging", "f_center_hz": 930.5e6}"#,
    );
    assert_eq!(st, 201, "{bm}");

    ctl.finish();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(handle.wait().map_err(|e| format!("{e:#}")));
    });
    let summary = rx
        .recv_timeout(Duration::from_secs(180))
        .expect("the run finishes")
        .unwrap();
    eprintln!("{}", summary.to_text());
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    assert_eq!(
        summary.counter("/chains/recordings"),
        1,
        "the FM recording only"
    );
    drop(server);

    // Audited: every control request, with the token id and old/new values.
    let audit: Vec<Value> = std::fs::read_to_string(dir.join("control-audit.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(audit.len(), 12, "one entry per control request: {audit:#?}");
    assert!(audit.iter().all(|e| e["token_id"] == json!(token_id)));
    let paging = audit
        .iter()
        .find(|e| e["action"] == "center" && e["new"]["center_hz"] == json!(930.5e6))
        .unwrap();
    assert_eq!(paging["old"]["center_hz"], json!(101.0e6));
    assert_eq!(paging["result"], "ok");

    // Persisted.
    let repo = Repository::open(dir.join("hackriff.db")).unwrap();
    assert_eq!(repo.bookmarks().unwrap()[0].name, "paging");
    drop(repo);
}

/// A recording with no samples at 100.8 MHz.
fn empty_recording(dir: &Path) -> PathBuf {
    use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("empty.sigmf-data"), []).unwrap();
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(2.4e6);
    meta.captures.push(Capture {
        sample_start: 0,
        frequency: Some(100.8e6),
        datetime: Some("2026-09-13T12:00:00Z".into()),
        provenance: None,
        clip_count: None,
        extra: serde_json::Map::new(),
    });
    let path = dir.join("empty.sigmf-meta");
    meta.write(&path).unwrap();
    path
}

#[test]
fn a_replayed_recording_refuses_device_settings_but_accepts_display_settings() {
    let dir = temp_data_dir();
    let _guard = TempDataDirGuard::new(dir.clone());
    let path = empty_recording(&dir.join("src"));
    let Serving {
        server,
        handle,
        live_control,
        ..
    } = start(&ServeOptions {
        source: ServeSource::Replay {
            path,
            loop_replay: false,
            realtime: false,
        },
        data_dir: Some(dir.join("data")),
        bind: "127.0.0.1:0".parse().unwrap(),
        ui_dist: None,
        fft_len: 1024,
        rows_per_s: 25.0,
        calibration: None,
        token: Some(TOKEN.into()),
        listen: Default::default(),
        compute: Default::default(),
        iq_buffer: Default::default(),
    })
    .unwrap();
    assert!(live_control.is_none());
    let addr = server.local_addr();
    for (path, body) in [
        ("/api/control/center", r#"{"center_hz": 99.5e6}"#),
        ("/api/control/rate", r#"{"sample_rate_hz": 10e6}"#),
        ("/api/control/gains", r#"{"gains": {"lna": 24}}"#),
        ("/api/control/bias_tee", r#"{"enabled": true}"#),
    ] {
        let (st, v) = post(addr, path, body);
        assert_eq!(
            (st, v["code"].as_str()),
            (409, Some("not_live")),
            "{path}: {v}"
        );
    }
    let (st, v) = post(addr, "/api/control/display", r#"{"fft_size": 2048}"#);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["display"]["fft_size"], json!(2048));
    let (_, state) = call(addr, "GET", "/api/control/state", None);
    assert_eq!(state["live"], json!(false));
    assert_eq!(state["run"]["live"], json!(false));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(handle.wait().map_err(|e| format!("{e:#}")));
    });
    let summary = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the run over an empty recording finishes")
        .unwrap();
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    drop(server);
}
