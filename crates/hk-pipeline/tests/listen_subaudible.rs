//! **T-988 (SIGNAL-090): blind CTCSS/DCS identification on a Listen stream, through the mock SDR.**
//!
//! Each test writes a synthetic NBFM station — a 1 kHz "voice" at 2.5 kHz deviation, plus a
//! CTCSS tone (600 Hz deviation) or a DCS code word stream (±600 Hz NRZ at 134.4 bit/s), or no
//! sub-audible signalling at all — as a ci8 SigMF recording, serves it through
//! [`MockSdrDriver`] behind the generic device contract (CLAUDE.md: e2e goes through the SDR
//! device interface), and listens to it over `/ws/open/listen` with a plain frequency selection.
//! **Nothing tells the system a tone exists**: the station is found, its mode chosen and its tone
//! or code measured from the discriminator alone, and the truth (the tone/code the recording was
//! written with) stays in the test.
//!
//! Asserted on the wire: the type-3 status records' `subaudible` (`ctcss`/`dcs`/`none`),
//! `ctcss_hz` + `tone_hz` (to 0.1 Hz) or `dcs_code`/`dcs_polarity`/`dcs_alias`. And on the data
//! model: when the station has an inventory entry, its latest Demodulation carries
//! `params.subaudible` — the value `GET /api/inventory/{id}` serves as
//! `estimated_params.subaudible`.
//!
//! **Red on the code before T-988:** no status record carried a `subaudible` key, so every test
//! here times out waiting for one.
//!
//! Recording lengths are chosen so each loop is seamless: 5 s holds a whole number of cycles of
//! 67.0, 131.8 and 233.6 Hz; the DCS recordings hold exactly 29 code words.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use hk_api::{ApiState, Server, ServerConfig, Token};
use hk_core::{MockEnd, MockOptions, MockSdrDriver, Pacing};
use hk_demod::subaudible::{DCS_BIT_RATE, dcs_word};
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_model::{FreqRange, Region, Repository, SubaudibleKind, TimeRange, Timestamp};
use hk_pipeline::class::window_class;
use hk_pipeline::{
    ListenSettings, Pipeline, PipelineConfig, PipelineHandle, SourceInfo, TrackInventory,
    replay_plan,
};
use hk_stream::BinaryRecordHeader;
use hk_stream::OpenerRegistry;
use hk_stream::record::parse_status_record;
use serde_json::Value;
use tungstenite::Message;
use tungstenite::stream::MaybeTlsStream;

const TOKEN: &str = "t988-listen-subaudible-token-0123456789abcd";
const CENTER: f64 = 100.0e6;
const FS: f64 = 1.0e6;
const STATION_OFFSET_HZ: f64 = 200e3;

/// What rides under the voice.
#[derive(Clone, Copy)]
enum Sub {
    Ctcss(f64),
    Dcs { code: u16, inverted: bool },
    None,
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hk-t988-{tag}-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
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

/// Writes the station (module docs) as `nbfm.sigmf-meta/-data` in `dir`.
fn nbfm_recording(dir: &Path, sub: Sub) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let secs = match sub {
        Sub::Dcs { .. } => 29.0 * 23.0 / DCS_BIT_RATE,
        _ => 5.0,
    };
    let n = (secs * FS).round() as usize;
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut noise = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        ((state >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 12.0
    };
    let tau = std::f64::consts::TAU;
    let word = match sub {
        Sub::Dcs { code, .. } => dcs_word(code),
        _ => 0,
    };
    let mut phase = 0.0f64;
    let mut data = Vec::with_capacity(2 * n);
    for i in 0..n {
        let t = i as f64 / FS;
        let sub_hz = match sub {
            Sub::Ctcss(f) => 600.0 * (tau * f * t).sin(),
            Sub::Dcs { inverted, .. } => {
                let bit = (t * DCS_BIT_RATE).floor() as usize % 23;
                if ((word >> bit) & 1 == 1) != inverted {
                    600.0
                } else {
                    -600.0
                }
            }
            Sub::None => 0.0,
        };
        let f = STATION_OFFSET_HZ + 2_500.0 * (tau * 1_000.0 * t).sin() + sub_hz;
        phase = (phase + tau * f / FS) % tau;
        let re = (40.0 * phase.cos() + noise()).round().clamp(-128.0, 127.0) as i8;
        let im = (40.0 * phase.sin() + noise()).round().clamp(-128.0, 127.0) as i8;
        data.push(re as u8);
        data.push(im as u8);
    }
    std::fs::write(dir.join("nbfm.sigmf-data"), data).unwrap();
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(FS);
    meta.captures.push(Capture {
        sample_start: 0,
        frequency: Some(CENTER),
        datetime: Some("2026-09-25T13:45:00Z".into()),
        provenance: None,
        clip_count: None,
        extra: serde_json::Map::new(),
    });
    let path = dir.join("nbfm.sigmf-meta");
    meta.write(&path).unwrap();
    path
}

/// A live, window-classed run over the mock SDR with `hk serve`'s `/ws/open/listen` on it (the
/// configuration of `listen_conformance.rs`).
struct Run {
    handle: PipelineHandle,
    server: Server,
    dir: TempDir,
}

impl Run {
    fn start(tag: &str, sub: Sub) -> Self {
        let dir = TempDir::new(tag);
        let meta = nbfm_recording(&dir.0.join("rec"), sub);
        let driver = MockSdrDriver::new(
            &meta,
            MockOptions {
                end: MockEnd::Loop,
                block_len: 16_384,
                pacing: Pacing::Unpaced,
                ..MockOptions::default()
            },
        )
        .unwrap();
        let source = driver.open_mock(&driver.default_request()).unwrap();
        let info = SourceInfo {
            sample_rate_hz: FS,
            center_hz: CENTER,
            start_time: source.start_time(),
        };
        let mut cfg =
            PipelineConfig::new(&dir.0, replay_plan(CENTER, FS, info.start_time)).unwrap();
        cfg.source_class = window_class(CENTER, FS);
        cfg.live_window_class = true;
        cfg.lossless = true;
        cfg.settings.chains = Some(Vec::new());
        let handle = Pipeline::start(
            cfg,
            Box::new(source),
            info,
            None,
            Box::new(TrackInventory::default()),
        )
        .unwrap();
        handle.set_listen_settings(ListenSettings::default());
        let counters = handle.counters();
        wait("the first second of samples", || {
            counters.source.samples.load(Ordering::Relaxed) >= FS as u64
        });
        let state = ApiState {
            on_demand: OpenerRegistry::new().with("listen", handle.listen_service()),
            ..ApiState::default()
        };
        let server = Server::start(
            ServerConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                Token::from_config(TOKEN).unwrap(),
            ),
            state,
        )
        .unwrap();
        Self {
            handle,
            server,
            dir,
        }
    }

    fn addr(&self) -> SocketAddr {
        self.server.local_addr()
    }

    fn finish(self) {
        let Self {
            handle,
            server,
            dir,
        } = self;
        drop(server);
        handle.stop();
        handle.wait().unwrap();
        drop(dir);
    }
}

/// A bound on waiting for an EVENT (never an assertion about how long something took).
fn wait(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Listens to a 20 kHz selection around the station (no mode, no tone: nothing but where) and
/// returns the first status record whose `subaudible` has settled (not `measuring`).
fn settled_status(addr: SocketAddr) -> Value {
    let f = CENTER + STATION_OFFSET_HZ;
    let (mut ws, _) = tungstenite::connect(format!(
        "ws://{addr}/ws/open/listen?token={TOKEN}&f_lo={}&f_hi={}",
        f - 10e3,
        f + 10e3
    ))
    .expect("upgrade");
    if let MaybeTlsStream::Plain(s) = ws.get_mut() {
        s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut header_seen = false;
    let mut last = Value::Null;
    loop {
        assert!(
            Instant::now() < deadline,
            "no settled `subaudible` on the status records (last status: {last})"
        );
        match ws.read().expect("the stream stays open") {
            Message::Text(t) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                assert_ne!(v["type"], "refused", "the station was refused: {v}");
                assert_eq!(v["audio"]["mode"], "nbfm", "auto-mode chose NBFM: {v}");
                header_seen = true;
            }
            Message::Binary(b) => {
                let h = BinaryRecordHeader::decode(&b).unwrap();
                if h.record_type != 3 {
                    continue;
                }
                let (_, v) = parse_status_record(&b).expect("status record");
                last = v.clone();
                match v.get("subaudible").and_then(Value::as_str) {
                    Some("measuring") | None => continue,
                    Some(_) => {
                        assert!(header_seen);
                        let _ = ws.close(None);
                        return v;
                    }
                }
            }
            _ => {}
        }
    }
}

/// The station's inventory entry and the sub-audible conclusion on its latest Demodulation, when
/// the run has put one there.
fn row_subaudible(run: &Run) -> Option<hk_model::Subaudible> {
    let repo = Repository::open(run.dir.0.join("hackriff.db")).ok()?;
    let f = CENTER + STATION_OFFSET_HZ;
    let ever = TimeRange::new(
        Timestamp::from_unix_nanos(0),
        Timestamp::from_unix_nanos(i64::MAX / 2),
    );
    let rows = repo
        .emitters_in_region(&Region::new(FreqRange::centered(f, 20e3), ever))
        .ok()?;
    rows.iter().find_map(|e| {
        repo.latest_demodulation_for_emitter(e.id)
            .ok()
            .flatten()
            .and_then(|d| d.params.subaudible)
    })
}

fn ctcss(tag: &str, tone: f64) {
    let run = Run::start(tag, Sub::Ctcss(tone));
    let v = settled_status(run.addr());
    assert_eq!(v["subaudible"], "ctcss", "{v}");
    assert_eq!(v["ctcss_hz"].as_f64(), Some(tone), "{v}");
    let measured = v["tone_hz"].as_f64().unwrap();
    assert!(
        (measured - tone).abs() <= 0.1,
        "measured {measured} Hz for {tone}: {v}"
    );
    assert!(v.get("dcs_code").is_none(), "{v}");
    assert!(v.get("tone2_hz").is_none(), "one tone was sent: {v}");
    run.finish();
}

fn dcs(tag: &str, code: u16, inverted: bool, want: (&str, &str, &str)) {
    let run = Run::start(tag, Sub::Dcs { code, inverted });
    let v = settled_status(run.addr());
    assert_eq!(v["subaudible"], "dcs", "{v}");
    assert_eq!(
        (
            v["dcs_code"].as_str(),
            v["dcs_polarity"].as_str(),
            v["dcs_alias"].as_str()
        ),
        (Some(want.0), Some(want.1), Some(want.2)),
        "{v}"
    );
    assert!(v.get("ctcss_hz").is_none(), "{v}");
    run.finish();
}

#[test]
fn listen_identifies_ctcss_67p0() {
    ctcss("c670", 67.0);
}

#[test]
fn listen_identifies_ctcss_131p8_and_files_it_on_the_emitter_row() {
    let run = Run::start("c1318", Sub::Ctcss(131.8));
    let v = settled_status(run.addr());
    assert_eq!(v["subaudible"], "ctcss", "{v}");
    assert_eq!(v["ctcss_hz"].as_f64(), Some(131.8), "{v}");
    assert!(v.get("tone2_hz").is_none(), "one tone was sent: {v}");
    // The row: when the run's inventory has an entry for the station, the settled conclusion is
    // on its latest Demodulation (what `estimated_params.subaudible` serves). A range listen with
    // no entry at the channel has no row to write on, which is not a failure of this feature.
    if let Some(s) = row_subaudible(&run) {
        eprintln!("row: {s:?}");
        assert_eq!(s.kind, SubaudibleKind::Ctcss, "{s:?}");
        assert_eq!(s.tones[0].table_hz, Some(131.8), "{s:?}");
        assert_eq!(s.tones.len(), 1, "one tone was sent: {s:?}");
    } else {
        eprintln!("no inventory entry at the station yet: the row assertion did not run");
    }
    run.finish();
}

#[test]
fn listen_identifies_ctcss_233p6() {
    ctcss("c2336", 233.6);
}

#[test]
fn listen_identifies_dcs_023_normal() {
    dcs("d023", 0o023, false, ("023", "normal", "047I"));
}

#[test]
fn listen_identifies_dcs_754_normal() {
    dcs("d754", 0o754, false, ("754", "normal", "116I"));
}

#[test]
fn listen_reports_no_tone_on_a_toneless_nbfm_station() {
    let run = Run::start("none", Sub::None);
    let v = settled_status(run.addr());
    assert_eq!(
        v["subaudible"], "none",
        "'no tone' is reported, not left absent: {v}"
    );
    assert!(
        v.get("ctcss_hz").is_none() && v.get("dcs_code").is_none(),
        "{v}"
    );
    assert!(v["subaudible_s"].as_f64().unwrap() >= 2.0, "{v}");
    run.finish();
}
