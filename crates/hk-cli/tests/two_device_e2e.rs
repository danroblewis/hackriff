//! T-513 — two mock front ends, through the device interface, over the real HTTP API (milestone
//! MSDR, use case AWARE-011). CLAUDE.md's e2e rule: this drives the system through
//! [`hk_core::Source`] (two [`hk_core::MockSdrDriver`] instances, one per band), never by feeding
//! files straight into the pipeline.
//!
//! T-510 already proved attribution stays apart at the pipeline/store level for two front ends on
//! **disjoint** bands. This ticket adds the case T-510 could not: two front ends whose tuned
//! windows **overlap**, with one tone inside the overlap (shared air) and one tone in each front
//! end's own band alone (device-local physics). It asserts, through the wire a real client reads:
//!
//! - both front ends appear in `GET /api/navigation`'s `windows[]`;
//! - both appear in `GET /api/coverage`'s `devices[]`, and each one's own coverage plane covers
//!   only its own band — the "grey is honest" invariant, exercised over two front ends rather than
//!   one;
//! - both appear in `GET /api/tiles`'s `coverage.devices[]` too;
//! - the tone **both** radios were tuned over resolves to **one** inventory entry, and detections
//!   naming both device ids fall inside that entry's occupied band (shared air, merged);
//! - the tone **only one** radio was tuned over stays its **own** entry, and no detection naming
//!   the other radio ever falls inside it (device-local physics never crosses).
//!
//! Blind: nothing here looks a frequency up in a database or tunes from an expectation; every
//! assertion is against what the run actually produced.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use hk_api::{LiveControls, LiveTuning, Server, SourceLiveControl, StreamRegistry, Token};
use hk_cli::pipeline::serve_api;
use hk_core::{MockEnd, Pacing, Source};
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_model::{
    BiasTee, ClockSource, FreqRange, InventoryQuery, Provenance, Region, Repository, TimeRange,
    Timestamp, TimestampMethod, Tune,
};
use hk_pipeline::{
    ExtraSource, Pipeline, PipelineConfig, TrackInventory, open_mock_replay, replay_plan,
};
use serde_json::Value;

const TOKEN: &str = "t513-two-device-token-0123456789abcdef";

/// Device A: tuned to 99.7 MHz. Carries [`A_ONLY_HZ`] (its own alone) and [`SHARED_HZ`] (the air
/// device B also sees).
const DEVICE_A: &str = "unit-a";
const CENTER_A: f64 = 99.7e6;
const FS_A: f64 = 3.0e6;

/// Device B: tuned to 101.0 MHz, overlapping A's window by 900 kHz. Carries [`SHARED_HZ`] and
/// [`B_ONLY_HZ`] (its own alone).
const DEVICE_B: &str = "unit-b";
const CENTER_B: f64 = 101.0e6;
const FS_B: f64 = 3.0e6;

const SECS: f64 = 6.0;

/// Inside A's window only ([98.2, 101.2] MHz): outside B's ([99.5, 102.5] MHz).
const A_ONLY_HZ: f64 = 98.7e6;
/// Inside both windows: the shared air.
const SHARED_HZ: f64 = 100.3e6;
/// Inside B's window only: outside A's.
const B_ONLY_HZ: f64 = 102.0e6;

/// How close a detection or an emitter's centre must be to one of the three tones above to count
/// as "that tone" rather than noise or another tone (the tones are >=1.5 MHz apart; this is far
/// tighter than that, and far looser than a CW tone's actual measured centre error).
const FREQ_TOL_HZ: f64 = 50e3;

fn mock_id(device: &str) -> String {
    format!("mock:{device}")
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hk-cli-t513-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn provenance_for(device_id: &str, center_hz: f64, fs: f64) -> Provenance {
    Provenance {
        device_id: device_id.into(),
        tune: Tune {
            center_hz,
            sample_rate_hz: fs,
            lna_db: 24.0,
            vga_db: 20.0,
            amp_on: false,
            bandwidth_hz: fs,
        },
        overload: false,
        quantisation_limited: false,
        noise_sigma_lsb: None,
        temperature_c: None,
        antenna_port: None,
        bias_tee: BiasTee::Unknown,
        clock_source: ClockSource::Internal,
        clock_locked: true,
        calibration_state_ref: None,
        spur_mask_ref: None,
        timestamp_method: TimestampMethod::Synthetic,
        timestamp_error_budget_ns: None,
        capture_artefacts: Vec::new(),
    }
}

/// Writes `secs` of noise at `fs`/`center_hz`, plus one tone per entry of `tones` (absolute
/// frequency, amplitude), as `<name>.sigmf-meta/-data` in `dir`. The capture's own
/// `hackriff:provenance` names `device_id`, so the mock replaying it calls itself
/// `mock:<device_id>`, and every recording shares `start_iso` so the two front ends share a clock
/// (`hk_pipeline::MAX_START_SKEW`).
fn device_recording(
    dir: &Path,
    name: &str,
    center_hz: f64,
    fs: f64,
    secs: f64,
    tones: &[(f64, f64)],
    start_iso: &str,
) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let n = (secs * fs) as usize;
    let mut state = 0x2545_f491_4f6c_dd1du64 ^ (name.len() as u64).wrapping_mul(0x9e37_79b9);
    let mut noise = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        ((state >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 12.0
    };
    let mut data = Vec::with_capacity(2 * n);
    for i in 0..n {
        let mut re = noise();
        let mut im = noise();
        for &(f_hz, amp) in tones {
            let ph = 2.0 * std::f64::consts::PI * (f_hz - center_hz) * i as f64 / fs;
            re += amp * ph.cos();
            im += amp * ph.sin();
        }
        data.push(re.round().clamp(-128.0, 127.0) as i8 as u8);
        data.push(im.round().clamp(-128.0, 127.0) as i8 as u8);
    }
    std::fs::write(dir.join(format!("{name}.sigmf-data")), data).unwrap();
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(fs);
    meta.captures = vec![Capture {
        sample_start: 0,
        frequency: Some(center_hz),
        datetime: Some(start_iso.into()),
        provenance: Some(provenance_for(name, center_hz, fs)),
        clip_count: None,
        extra: Default::default(),
    }];
    let path = dir.join(format!("{name}.sigmf-meta"));
    meta.write(&path).unwrap();
    path
}

fn window(start: Timestamp, secs: f64) -> TimeRange {
    TimeRange::new(
        start.saturating_add_nanos(-30_000_000_000),
        start.saturating_add_nanos((secs * 1e9) as i64 + 30_000_000_000),
    )
}

fn call(addr: std::net::SocketAddr, path: &str) -> (u16, Value) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(60)))
        .unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TOKEN}\r\n\
         Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let status = raw[9..12].parse().unwrap();
    let body = raw.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

/// A validated tile address's frequency search, general over whatever the run's own view lattice
/// doubling turns out to be (T-505/T-513): grows `level_f` until a tile at the computed `f_index`
/// covers `[needed_lo, needed_hi]` end to end, discovering the lattice's own finest cell (`f0`)
/// from a first probe rather than assuming a constant.
fn find_freq_tile(
    addr: std::net::SocketAddr,
    needed_lo: f64,
    needed_hi: f64,
    cells: usize,
) -> (u32, i64, f64) {
    let (st, probe) = call(
        addr,
        &format!("/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells={cells}"),
    );
    assert_eq!(st, 200, "{probe}");
    let f0 = probe["extent"]["f_cell_hz"].as_f64().unwrap();
    assert!(f0 > 0.0, "{probe}");
    let mut level_f = 0u32;
    loop {
        let span = f0 * 2f64.powi(level_f as i32) * cells as f64;
        let f_index = (needed_lo / span).floor() as i64;
        let tile_lo = f_index as f64 * span;
        let tile_hi = tile_lo + span;
        if tile_lo <= needed_lo && tile_hi >= needed_hi {
            return (level_f, f_index, span);
        }
        level_f += 1;
        assert!(level_f < 30, "no level_f tiles [{needed_lo}, {needed_hi}]");
    }
}

/// The time-axis twin of [`find_freq_tile`].
fn find_time_tile(
    addr: std::net::SocketAddr,
    needed_lo_ns: i64,
    needed_hi_ns: i64,
    cells: usize,
) -> (u32, i64, i64) {
    let (st, probe) = call(
        addr,
        &format!("/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells={cells}"),
    );
    assert_eq!(st, 200, "{probe}");
    let t0 = (probe["extent"]["t_cell_s"].as_f64().unwrap() * 1e9).round() as i64;
    assert!(t0 > 0, "{probe}");
    let mut level_t = 0u32;
    loop {
        let span = t0
            .saturating_mul(1i64 << level_t)
            .saturating_mul(cells as i64);
        let t_index = needed_lo_ns.div_euclid(span);
        let tile_lo = t_index * span;
        let tile_hi = tile_lo + span;
        if tile_lo <= needed_lo_ns && tile_hi >= needed_hi_ns {
            return (level_t, t_index, span);
        }
        level_t += 1;
        assert!(
            level_t < 30,
            "no level_t tiles [{needed_lo_ns}, {needed_hi_ns}]"
        );
    }
}

#[test]
fn two_overlapping_mock_front_ends_merge_shared_air_and_keep_physics_apart() {
    let rec = TempDir::new("recordings");
    let dir = TempDir::new("run");
    let start_iso = "2024-06-01T00:02:00Z";

    let a = device_recording(
        &rec.0,
        DEVICE_A,
        CENTER_A,
        FS_A,
        SECS,
        &[(A_ONLY_HZ, 40.0), (SHARED_HZ, 40.0)],
        start_iso,
    );
    let b = device_recording(
        &rec.0,
        DEVICE_B,
        CENTER_B,
        FS_B,
        SECS,
        &[(SHARED_HZ, 40.0), (B_ONLY_HZ, 40.0)],
        start_iso,
    );

    let primary = open_mock_replay(&a, Pacing::Unpaced, MockEnd::Stop).unwrap();
    let second = open_mock_replay(&b, Pacing::Unpaced, MockEnd::Stop).unwrap();
    assert_eq!(primary.device.device_id, mock_id(DEVICE_A));
    assert_eq!(second.device.device_id, mock_id(DEVICE_B));
    let start = primary.info.start_time;

    let mut cfg = PipelineConfig::new(&dir.0, replay_plan(CENTER_A, FS_A, start)).unwrap();
    cfg.lossless = true;
    cfg.settings.chains = Some(Vec::new());
    cfg.iq_buffer.enabled = Some(true);
    cfg.iq_buffer.retention_s = 2.0;
    cfg.iq_buffer.max_bytes = Some(64 << 20);
    cfg.live_window_class = true;
    cfg.source_class = primary.class;
    cfg.device_id = primary.device.device_id.clone();

    let handle = Pipeline::start_multi(
        cfg,
        Box::new(primary.source) as Box<dyn Source>,
        primary.info,
        None,
        Box::new(TrackInventory::default()),
        vec![ExtraSource {
            source: Box::new(second.source) as Box<dyn Source>,
            info: second.info,
        }],
    )
    .unwrap();

    let devices = handle.devices();
    assert_eq!(devices.len(), 2, "one Pipeline, two front ends");
    let live_controls: Vec<Arc<dyn hk_api::LiveControl>> = devices
        .iter()
        .map(|d| {
            let (center_hz, sample_rate_hz) = if d.primary {
                (CENTER_A, FS_A)
            } else {
                (CENTER_B, FS_B)
            };
            Arc::new(SourceLiveControl::new(
                Arc::clone(&d.control),
                LiveTuning {
                    center_hz,
                    sample_rate_hz,
                    gains: Vec::new(),
                    bias_tee: BiasTee::Unknown,
                    baseband_filter_hz: None,
                },
            )) as Arc<dyn hk_api::LiveControl>
        })
        .collect();
    drop(devices);
    let live_controls = LiveControls::new(live_controls).unwrap();

    let registry = StreamRegistry::new();
    let token = Token::from_config(TOKEN).unwrap();
    let server: Server = serve_api(
        "127.0.0.1:0".parse().unwrap(),
        None,
        &registry,
        &handle,
        token,
        "t513",
        live_controls,
    )
    .unwrap();
    let addr = server.local_addr();

    let summary = handle.wait().unwrap();
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);

    // --- /api/navigation: both front ends' windows ---
    let (st, nav) = call(addr, "/api/navigation");
    assert_eq!(st, 200, "{nav}");
    let windows = nav["windows"].as_array().expect("windows[] is a list");
    let ids: Vec<&str> = windows
        .iter()
        .filter_map(|w| w["device_id"].as_str())
        .collect();
    assert!(
        ids.contains(&mock_id(DEVICE_A).as_str()) && ids.contains(&mock_id(DEVICE_B).as_str()),
        "both front ends must appear in /api/navigation windows[], got {ids:?}"
    );
    let win_a = windows
        .iter()
        .find(|w| w["device_id"] == mock_id(DEVICE_A).as_str())
        .unwrap();
    assert_eq!(win_a["center_hz"], serde_json::json!(CENTER_A));
    let win_b = windows
        .iter()
        .find(|w| w["device_id"] == mock_id(DEVICE_B).as_str())
        .unwrap();
    assert_eq!(win_b["center_hz"], serde_json::json!(CENTER_B));

    // --- /api/coverage: both front ends' devices[], each one honest about its own band ---
    let w = window(start, SECS);
    let cov_path = format!(
        "/api/coverage?f_lo={}&f_hi={}&cells=64&rows=1&t0={}&t1={}",
        97.0e6,
        103.0e6,
        w.start.as_unix_nanos() as f64 * 1e-9,
        w.end.as_unix_nanos() as f64 * 1e-9,
    );
    let (st, cov) = call(addr, &cov_path);
    assert_eq!(st, 200, "{cov}");
    let cov_devices = cov["devices"].as_array().expect("devices[]");
    let cov_ids: Vec<&str> = cov_devices
        .iter()
        .filter_map(|d| d["device"].as_str())
        .collect();
    assert!(
        cov_ids.contains(&mock_id(DEVICE_A).as_str())
            && cov_ids.contains(&mock_id(DEVICE_B).as_str()),
        "both front ends must appear in /api/coverage devices[], got {cov_ids:?}"
    );
    let f_lo_hz = cov["grid"]["f_lo_hz"].as_f64().unwrap();
    let f_cell_hz = cov["grid"]["f_cell_hz"].as_f64().unwrap();
    let col_of = |f_hz: f64| -> usize { ((f_hz - f_lo_hz) / f_cell_hz).floor() as usize };
    let state_at = |device: &str, f_hz: f64| -> String {
        let g = cov_devices
            .iter()
            .find(|d| d["device"] == device)
            .unwrap_or_else(|| panic!("{device} missing from /api/coverage devices[]"));
        let cells = g["cells"].as_array().unwrap();
        cells[col_of(f_hz)]["state"].as_str().unwrap().to_string()
    };
    let id_a = mock_id(DEVICE_A);
    let id_b = mock_id(DEVICE_B);
    assert_eq!(
        state_at(&id_a, A_ONLY_HZ),
        "observed",
        "A must observe its own tone"
    );
    assert_eq!(
        state_at(&id_b, A_ONLY_HZ),
        "unobserved",
        "B must never read observed over A's own band: physics crossed"
    );
    assert_eq!(
        state_at(&id_b, B_ONLY_HZ),
        "observed",
        "B must observe its own tone"
    );
    assert_eq!(
        state_at(&id_a, B_ONLY_HZ),
        "unobserved",
        "A must never read observed over B's own band: physics crossed"
    );
    assert_eq!(
        state_at(&id_a, SHARED_HZ),
        "observed",
        "A must observe the shared air"
    );
    assert_eq!(
        state_at(&id_b, SHARED_HZ),
        "observed",
        "B must observe the shared air"
    );

    // --- /api/tiles: both front ends, over the same coverage the tile route serves ---
    let (level_f, f_index, f_span) = find_freq_tile(addr, 97.0e6, 103.0e6, 256);
    let start_ns = start.as_unix_nanos();
    let end_ns = start_ns + (SECS * 1e9) as i64;
    let (level_t, t_index, _t_span) = find_time_tile(addr, start_ns, end_ns, 256);
    let (st, tile) = call(
        addr,
        &format!(
            "/api/tiles?level_f={level_f}&level_t={level_t}&f_index={f_index}&t_index={t_index}&cells=256"
        ),
    );
    assert_eq!(st, 200, "{tile}");
    assert!(
        tile["extent"]["f_lo_hz"].as_f64().unwrap() <= A_ONLY_HZ - FREQ_TOL_HZ
            && tile["extent"]["f_hi_hz"].as_f64().unwrap() >= B_ONLY_HZ + FREQ_TOL_HZ,
        "the addressed tile must span every tone: {tile}"
    );
    assert!(f_span > 0.0);
    let tile_devices = tile["coverage"]["devices"]
        .as_array()
        .expect("coverage.devices[]");
    let tile_ids: Vec<&str> = tile_devices
        .iter()
        .filter_map(|d| d["device"].as_str())
        .collect();
    assert!(
        tile_ids.contains(&id_a.as_str()) && tile_ids.contains(&id_b.as_str()),
        "both front ends must appear in /api/tiles' coverage.devices[], got {tile_ids:?}"
    );

    // --- The inventory: shared air merges, device-local physics never crosses ---
    let repo = Repository::open(dir.0.join("hackriff.db")).unwrap();
    let page = repo
        .query_inventory(&InventoryQuery {
            freq: Some(FreqRange::new(1e6, 6e9)),
            time: Some(w),
            limit: 500,
            ..InventoryQuery::default()
        })
        .unwrap();
    let entry_near = |target: f64| -> Vec<_> {
        page.entries
            .iter()
            .filter(|e| (e.emitter.f_center_hz - target).abs() < FREQ_TOL_HZ)
            .collect()
    };
    let shared_entries = entry_near(SHARED_HZ);
    assert_eq!(
        shared_entries.len(),
        1,
        "the shared tone must resolve to exactly one inventory entry, got {} near {SHARED_HZ}: {:?}",
        shared_entries.len(),
        shared_entries
            .iter()
            .map(|e| e.emitter.f_center_hz)
            .collect::<Vec<_>>()
    );
    let a_entries = entry_near(A_ONLY_HZ);
    assert_eq!(a_entries.len(), 1, "A's own tone must resolve to one entry");
    let b_entries = entry_near(B_ONLY_HZ);
    assert_eq!(b_entries.len(), 1, "B's own tone must resolve to one entry");
    assert_ne!(shared_entries[0].emitter.id, a_entries[0].emitter.id);
    assert_ne!(shared_entries[0].emitter.id, b_entries[0].emitter.id);
    assert_ne!(a_entries[0].emitter.id, b_entries[0].emitter.id);

    // Every detection is attributed to the front end whose tuned window contains it (T-510's own
    // invariant, re-checked here over overlapping windows), and the ones near each tone name the
    // radios this ticket says should — both for the shared entry, one each for the private ones.
    let all = repo
        .detections_in_region(&Region::new(FreqRange::new(1e6, 6e9), w))
        .unwrap();
    let devices_of = |target: f64| -> std::collections::BTreeSet<String> {
        all.iter()
            .filter(|d| (d.f_center_hz - target).abs() < FREQ_TOL_HZ)
            .map(|d| repo.provenance(d.provenance_ref).unwrap().device_id)
            .collect()
    };
    let shared_devices = devices_of(SHARED_HZ);
    assert_eq!(
        shared_devices,
        [id_a.clone(), id_b.clone()].into_iter().collect(),
        "the shared tone must carry both front ends' provenance"
    );
    let a_devices = devices_of(A_ONLY_HZ);
    assert_eq!(
        a_devices,
        [id_a.clone()].into_iter().collect(),
        "A's own tone must carry only A's provenance: device-local physics crossed"
    );
    let b_devices = devices_of(B_ONLY_HZ);
    assert_eq!(
        b_devices,
        [id_b.clone()].into_iter().collect(),
        "B's own tone must carry only B's provenance: device-local physics crossed"
    );

    drop(server);
}
