//! T-022a HTTP endpoints: `/api/history` serves the T-017 region-over-time query (AWARE-042),
//! `/api/floor` serves the T-021 calibrated floor product (SPACE-050), and unauthenticated calls
//! are rejected.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hk_api::stream::{Publisher, PublisherConfig, StreamHeader, StreamKind};
use hk_api::{ApiState, Server, ServerConfig, StreamRegistry, Token};
use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::floor::{FloorConfig, NoiseFloorTracker};
use hk_dsp::radiometry::{PowerCalibrations, SyntheticCalSegment, synthetic_calibration_state};
use hk_dsp::synth::{Rng, complex_noise};
use hk_dsp::{InputInfo, StftConfig, StftProcessor, WelchConfig};
use hk_model::{
    ContentClass, FreqRange, GainSetting, PowerUnit, Provenance, SampleTime, TimeRange, Timestamp,
};
use hk_store::{
    FloorProduct, FloorProductConfig, FrameInput, Pyramid, PyramidConfig, RegionQuery, Resolution,
};
use serde_json::{Value, json};

const AWARE_042: &str = "AWARE-042";
const SPACE_050: &str = "SPACE-050";
const TOKEN: &str = "t022a-http-token-0123456789abcdef";
const S: i64 = 1_000_000_000;
/// 2026-09-13T12:00:00Z.
const T0_S: i64 = 1_789_300_800;
const T0: i64 = T0_S * S;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("hk-api-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ts(ns: i64) -> Timestamp {
    Timestamp::from_unix_nanos(ns)
}

fn serve(state: ApiState, ui_dist: Option<PathBuf>) -> Server {
    let mut config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(TOKEN).unwrap(),
    );
    config.ui_dist = ui_dist;
    Server::start(config, state).unwrap()
}

/// `GET path` with an optional `Authorization` value; returns the status and the body.
fn get(addr: SocketAddr, path: &str, authorization: Option<&str>) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let auth = authorization.map_or(String::new(), |a| format!("Authorization: {a}\r\n"));
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: test\r\n{auth}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response head");
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap();
    (status, raw[split + 4..].to_vec())
}

fn get_json(addr: SocketAddr, path: &str) -> Value {
    let (status, body) = get(addr, path, Some(&format!("Bearer {TOKEN}")));
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, 200, "{path}: {v}");
    v
}

fn f32_or_nan(v: &Value) -> f32 {
    v.as_f64().map_or(f32::NAN, |x| x as f32)
}

fn same_f32(a: f32, b: f32) -> bool {
    (a.is_nan() && b.is_nan()) || a == b
}

/// A two-channel duty-cycle scenario folded straight into a pyramid: 120 s at 10 frames/s, 64 bins
/// of 3125 Hz over 200 kHz at 446 MHz, noise density × gamma(16) (a 16-average periodogram).
/// Channel A (bins 10–12) is on during the first 10 s of every 20 s (duty 0.5); channel B
/// (bins 40–42) during the first 12 s of every minute (duty 0.2); both +30 dB.
fn duty_cycle_pyramid(dir: &TempDir) -> Pyramid {
    const NB: usize = 64;
    const BW: f64 = 3125.0;
    let f_lo = 446e6 - 100e3;
    let n0 = 10f64.powf(-4.0) / 200e3;
    let mut p = Pyramid::open(&dir.0, PyramidConfig::default()).unwrap();
    let mut rng = Rng::new(0x0042_0042);
    let mut psd = vec![0f32; NB];
    for k in 0..1200i64 {
        let sec = k / 10;
        let a_on = sec % 20 < 10;
        let b_on = sec % 60 < 12;
        for (i, v) in psd.iter_mut().enumerate() {
            let g: f64 = (0..16).map(|_| rng.gaussian_pair().0.powi(2)).sum::<f64>() / 16.0;
            let on = (a_on && (10..13).contains(&i)) || (b_on && (40..43).contains(&i));
            *v = (n0 * g + if on { 1000.0 * n0 } else { 0.0 }) as f32;
        }
        p.ingest(&FrameInput::new(
            ts(T0 + k * S / 10),
            S / 10,
            f_lo,
            BW,
            PowerUnit::Dbfs,
            &psd,
        ))
        .unwrap();
    }
    p.seal_through(ts(T0 + 3600 * S)).unwrap();
    p
}

#[test]
fn aware_042_history_endpoint_serves_the_region_query() {
    let dir = TempDir::new("aware042");
    let pyramid = Arc::new(Mutex::new(duty_cycle_pyramid(&dir)));
    let server = serve(
        ApiState {
            history: Some(Arc::clone(&pyramid)),
            ..ApiState::default()
        },
        None,
    );
    let (f_lo, f_hi) = (446e6 - 100e3, 446e6 + 100e3);
    let v = get_json(
        server.local_addr(),
        &format!(
            "/api/history?f_lo={f_lo}&f_hi={f_hi}&t0={T0_S}&t1={}&max_cells=100000",
            T0_S + 120
        ),
    );

    // The endpoint is the T-017 query, cell for cell.
    let level = v["level"].as_u64().unwrap() as u8;
    assert_eq!(level, 0, "{AWARE_042}: 120 s × 200 kHz fits level 0");
    let direct = pyramid
        .lock()
        .unwrap()
        .query(&RegionQuery {
            freq: FreqRange::new(f_lo, f_hi),
            time: TimeRange::new(ts(T0), ts(T0 + 120 * S)),
            resolution: Resolution::Level(level),
        })
        .unwrap();
    assert_eq!(v["nt"], json!(direct.nt));
    assert_eq!(v["nf"], json!(direct.nf));
    assert_eq!(v["f_cell_hz"], json!(direct.f_cell_hz));
    assert_eq!(v["t_cell_s"], json!(direct.t_cell_ns as f64 / 1e9));
    assert_eq!(
        v["t0_s"].as_f64().unwrap(),
        direct.time_of(0).as_unix_nanos() as f64 / 1e9
    );
    assert_eq!(v["unit"], json!("dbfs"));
    for (i, c) in direct.cells.iter().enumerate() {
        for (key, want) in [
            ("max_db", c.max_db),
            ("mean_db", c.mean_db),
            ("p_low_db", c.p_low_db),
            ("p_high_db", c.p_high_db),
            ("occupancy", c.occupancy),
            ("coverage", c.coverage),
        ] {
            let got = f32_or_nan(&v[key][i]);
            assert!(
                same_f32(got, want),
                "{AWARE_042}: {key}[{i}] {got} vs {want}"
            );
        }
        assert_eq!(v["frames"][i], json!(c.frames));
    }
    assert_eq!(v["provenance"]["frames"], json!(direct.provenance.frames));

    // AWARE-042: the duty cycle per channel reads back from the served grid.
    let nf = direct.nf;
    let f_first = v["f_lo_hz"].as_f64().unwrap();
    let f_cell = direct.f_cell_hz;
    let duty = |hz: f64| {
        let col = ((hz - f_first) / f_cell).floor() as usize;
        let occ: Vec<f64> = (0..direct.nt)
            .filter_map(|t| v["occupancy"][t * nf + col].as_f64())
            .collect();
        assert_eq!(occ.len(), direct.nt, "{AWARE_042}: every row observed");
        occ.iter().sum::<f64>() / occ.len() as f64
    };
    for (name, hz, truth) in [
        ("channel A", f_lo + 11.5 * 3125.0, 0.5),
        ("channel B", f_lo + 41.5 * 3125.0, 0.2),
        ("quiet", f_lo + 25.5 * 3125.0, 0.0),
    ] {
        let got = duty(hz);
        eprintln!("{AWARE_042}: {name} duty cycle {got:.3} (truth {truth})");
        assert!(
            (got - truth).abs() <= 0.02,
            "{AWARE_042}: {name} duty cycle {got:.3} vs truth {truth}"
        );
    }

    // A tighter cell budget moves to a coarser level (same rule the pyramid uses).
    let coarse = get_json(
        server.local_addr(),
        &format!(
            "/api/history?f_lo={f_lo}&f_hi={f_hi}&t0={T0_S}&t1={}&max_cells=64",
            T0_S + 120
        ),
    );
    assert!(coarse["level"].as_u64().unwrap() > 0);
    assert!(coarse["nt"].as_u64().unwrap() * coarse["nf"].as_u64().unwrap() <= 64);

    let (status, _) = get(
        server.local_addr(),
        &format!("/api/history?f_lo={f_hi}&f_hi={f_lo}&t0={T0_S}&t1={T0_S}"),
        Some(&format!("Bearer {TOKEN}")),
    );
    assert_eq!(status, 400, "inverted region refused");
}

#[test]
fn space_050_floor_endpoint_serves_the_calibrated_floor_product() {
    const FS: f64 = 250e3;
    const FC: f64 = 100e6;
    const K_DB: f64 = -70.0;
    const VARIANCE: f64 = 1e-3;
    const SECONDS: i64 = 12;
    let gain = GainSetting {
        lna_db: 24.0,
        vga_db: 20.0,
        amp_on: false,
    };
    let state = synthetic_calibration_state(
        "synthetic:t-022a",
        &[SyntheticCalSegment {
            band: FreqRange::centered(FC, FS),
            gain,
            k_db: K_DB,
        }],
        0.1,
        ts(T0),
    );
    let cals = PowerCalibrations::from_states([&state], None);
    let dir = TempDir::new("space050");
    let mut product = FloorProduct::open(&dir.0, FloorProductConfig::default(), cals).unwrap();
    let mut p: Provenance = serde_json::from_value(json!({
        "device_id": "synthetic:t-022a",
        "tune": {"center_hz": FC, "sample_rate_hz": FS, "lna_db": gain.lna_db, "vga_db": gain.vga_db,
                 "amp_on": gain.amp_on, "bandwidth_hz": 0.75 * FS},
        "overload": false, "quantisation_limited": false, "clock_source": "internal",
        "clock_locked": true, "timestamp_method": "synthetic", "timestamp_error_budget_ns": 0,
    }))
    .unwrap();
    p.calibration_state_ref = Some(state.id);
    let prov = ProvenanceHandle::new(p);
    let mut stft = StftProcessor::new(StftConfig::new(WelchConfig::new(1024), 32)).unwrap();
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut rng = Rng::new(0x0022_a050);
    let total = (SECONDS as f64 * FS) as u64;
    let mut index = 0u64;
    let mut flags = Discontinuity::STREAM_START;
    while index < total {
        let n = 25_000.min(total - index);
        let samples = complex_noise(&mut rng, n as usize, VARIANCE);
        let info = InputInfo {
            time: SampleTime {
                sample_index: index,
                host_time: ts(T0 + (index as f64 * 1e9 / FS) as i64),
            },
            discontinuity: std::mem::replace(&mut flags, Discontinuity::NONE),
            dropped_before: 0,
            provenance: &prov,
        };
        stft.push(info, &samples, |frame| {
            let f = tracker.update(frame, |_| {});
            product.ingest(frame, f).unwrap();
        });
        index += n;
    }
    product.seal_through(ts(T0 + 3600 * S)).unwrap();
    let product = Arc::new(Mutex::new(product));
    let server = serve(
        ApiState {
            floor: Some(Arc::clone(&product)),
            ..ApiState::default()
        },
        None,
    );
    let (lo, hi) = (FC - 0.4 * FS, FC + 0.4 * FS);
    let v = get_json(
        server.local_addr(),
        &format!(
            "/api/floor?f_lo={lo}&f_hi={hi}&t0={T0_S}&t1={}&max_steps=64",
            T0_S + SECONDS
        ),
    );

    // The endpoint is the T-021 product, step for step.
    let level = v["level"].as_u64().unwrap() as u8;
    let direct = product
        .lock()
        .unwrap()
        .floor_vs_time(
            FreqRange::new(lo, hi),
            ts(T0),
            ts(T0 + SECONDS * S),
            Resolution::Level(level),
        )
        .unwrap();
    let steps = v["steps"].as_array().unwrap();
    assert_eq!(steps.len(), direct.steps.len());
    assert!(steps.len() <= 64);
    for (j, (s, d)) in steps.iter().zip(&direct.steps).enumerate() {
        assert_eq!(s["t_s"].as_f64().unwrap(), d.t.as_unix_nanos() as f64 / 1e9);
        assert_eq!(s["value_db_per_hz"].as_f64(), d.value_db_per_hz, "step {j}");
        assert_eq!(s["unit"], json!(d.unit), "step {j}");
        assert_eq!(s["flags"], json!(d.flags.bits()), "step {j}");
        assert_eq!(
            s["uncertainty_db"].as_f64(),
            d.uncertainty_db.is_finite().then_some(d.uncertainty_db),
            "step {j}"
        );
    }
    assert_eq!(
        v["calibrated_provenance"]["calibration"],
        json!(state.id),
        "{SPACE_050}: the tile digest names the calibration"
    );

    // SPACE-050: the calibrated floor (dBm/Hz) matches the injected noise density + k.
    let want = 10.0 * (VARIANCE / FS).log10() + K_DB;
    let mut calibrated: Vec<f64> = steps
        .iter()
        .filter(|s| s["unit"] == json!("dbm"))
        .filter_map(|s| s["value_db_per_hz"].as_f64())
        .collect();
    assert!(
        calibrated.len() >= SECONDS as usize / 2,
        "{SPACE_050}: {} calibrated steps",
        calibrated.len()
    );
    calibrated.sort_by(f64::total_cmp);
    let median = calibrated[calibrated.len() / 2];
    eprintln!(
        "{SPACE_050}: served calibrated floor {median:.2} dBm/Hz vs injected {want:.2} (err {:+.2})",
        median - want
    );
    assert!(
        (median - want).abs() <= 1.0,
        "{SPACE_050}: {median:.2} vs {want:.2} dBm/Hz"
    );
    assert!(steps.iter().filter(|s| s["unit"] == json!("dbm")).all(|s| {
        s["noise_temperature_k"]
            .as_f64()
            .is_some_and(|k| k.is_finite() && k > 0.0)
    }));
}

#[test]
fn unauthenticated_api_calls_are_rejected() {
    let registry = StreamRegistry::new();
    let mut h = StreamHeader::new(
        "spectrum/a",
        StreamKind::Spectrum,
        ContentClass::Unrestricted,
        "hk-api-test",
    );
    h.datatype = Some("rf32_le".into());
    h.fft_size = Some(16);
    h.sample_rate_hz = Some(25.0);
    let publisher = Publisher::new(h, PublisherConfig::default()).unwrap();
    registry.register(publisher.header(), publisher.handle());
    let hist_dir = TempDir::new("auth-hist");
    let dist = TempDir::new("auth-dist");
    std::fs::write(
        dist.0.join("index.html"),
        "<!doctype html><title>ui</title>",
    )
    .unwrap();
    std::fs::write(hist_dir.0.join("secret.txt"), "not served").unwrap();
    let pyramid = Pyramid::open(hist_dir.0.join("p"), PyramidConfig::default()).unwrap();
    let server = serve(
        ApiState {
            streams: registry,
            history: Some(Arc::new(Mutex::new(pyramid))),
            floor: None,
        },
        Some(dist.0.clone()),
    );
    let addr = server.local_addr();
    let q = format!("f_lo=1e8&f_hi=1.001e8&t0={T0_S}&t1={}", T0_S + 10);
    let protected = [
        "/api/streams".to_owned(),
        format!("/api/history?{q}"),
        format!("/api/floor?{q}"),
        "/api/nope".to_owned(),
        "/ws/spectrum/a".to_owned(),
    ];
    for path in &protected {
        for auth in [
            None,
            Some("Bearer wrong-token-0123456789abcdef"),
            Some("Basic dXNlcjpwYXNz"),
            Some("Bearer "),
        ] {
            let (status, body) = get(addr, path, auth);
            assert_eq!(status, 401, "{path} with {auth:?}");
            let text = String::from_utf8_lossy(&body);
            assert!(!text.contains("spectrum/a"), "{path}: 401 reveals nothing");
        }
        let (status, _) = get(addr, &format!("{path}?token=wrong-token-0123456789"), None);
        assert_eq!(status, 401, "{path} with a wrong query token");
    }

    // With the token: listing is metadata only.
    let v = get_json(addr, "/api/streams");
    let s = &v["streams"][0];
    assert_eq!(s["stream_id"], json!("spectrum/a"));
    assert_eq!(s["kind"], json!("spectrum"));
    assert_eq!(s["content_class"], json!("unrestricted"));
    let (status, _) = get(addr, &format!("/api/streams?token={TOKEN}"), None);
    assert_eq!(status, 200, "query-parameter token");
    let (status, _) = get(
        addr,
        &format!("/api/history?{q}"),
        Some(&format!("Bearer {TOKEN}")),
    );
    assert_eq!(status, 200, "empty history answers");
    let (status, _) = get(
        addr,
        &format!("/api/floor?{q}"),
        Some(&format!("Bearer {TOKEN}")),
    );
    assert_eq!(status, 404, "no floor product configured");

    // Static UI: served without a token (code, no data), and confined to the dist directory.
    let (status, body) = get(addr, "/", None);
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&body).contains("<title>ui</title>"));
    for escape in [
        "/../auth-hist/secret.txt",
        "/%2e%2e/auth-hist/secret.txt",
        "/.hidden",
        "/index.html/../../x",
    ] {
        let (status, _) = get(addr, escape, None);
        assert_eq!(status, 404, "{escape}");
    }
    let (status, _) = get(addr, "/", Some("Bearer whatever"));
    assert_eq!(status, 200);
}
