//! `hk replay` and `hackriffd`: the binaries' pipeline composition (T-027). The pipeline itself
//! is `hk_pipeline`; this module picks the source, plan, data directory and API server.
//!
//! - `hk replay <fixture> [--data-dir D] [--plan P] [--serve ADDR] [--paced] [--schedule]` runs
//!   the recording once, unpaced and lossless by default, and prints the run summary. Without
//!   `--data-dir` a fresh directory under the system temp dir is used (printed). With `--serve`
//!   the API (streams, history, floor, status) is up during the run and stays up afterwards
//!   until the process is stopped.
//! - `hackriffd --source sigmf:<file> [--loop] --data-dir D [--plan P] [--bind ADDR]` paces the
//!   recording in real time (lossy like a live source: ring overruns are counted, never waited
//!   for), drives the attention scheduler with the plan, publishes streams over the bridge and
//!   serves `/api/status`. A live HackRF source is not implemented yet (the source stub).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use hk_api::{ApiState, Server, ServerConfig, StreamRegistry, Token};
use hk_core::{Pacing, Source};
use hk_model::{Repository, ScanPlan, Timestamp};
use hk_pipeline::{
    PipelineConfig, PipelineHandle, RunSummary, SourceFactory, SourceInfo, TrackInventory,
    open_replay, replay_plan,
};

/// `hk replay` options.
#[derive(Clone, Debug, Default)]
pub struct ReplayArgs {
    /// `.sigmf-meta` path.
    pub fixture: PathBuf,
    /// Data directory (default: a fresh temp directory).
    pub data_dir: Option<PathBuf>,
    /// ScanPlan JSON.
    pub plan: Option<PathBuf>,
    /// Serve the API while (and after) running.
    pub serve: Option<SocketAddr>,
    /// Real-time pacing (lossy) instead of unpaced lossless replay.
    pub paced: bool,
    /// Drive the attention scheduler (virtual tuning).
    pub schedule: bool,
    /// Offline feed cache for the correlator.
    pub feeds: Option<PathBuf>,
    /// Built UI directory.
    pub ui_dist: Option<PathBuf>,
}

/// `hackriffd` options.
#[derive(Clone, Debug)]
pub struct DaemonArgs {
    /// `sigmf:<path>` (or `hackrf`, not implemented yet).
    pub source: String,
    /// Replay again at the end, continuing the stream.
    pub loop_replay: bool,
    /// Data directory.
    pub data_dir: PathBuf,
    /// ScanPlan JSON.
    pub plan: Option<PathBuf>,
    /// API listen address.
    pub bind: SocketAddr,
    /// Built UI directory.
    pub ui_dist: Option<PathBuf>,
    /// Replay unpaced and lossless instead of in real time.
    pub unpaced: bool,
    /// Offline feed cache for the correlator.
    pub feeds: Option<PathBuf>,
    /// API token (default: `HK_TOKEN`, else generated).
    pub token: Option<String>,
}

/// Loads a ScanPlan JSON, or a single-region plan over the recording.
pub fn load_plan(path: Option<&Path>, info: &SourceInfo) -> anyhow::Result<ScanPlan> {
    match path {
        Some(p) => {
            let text =
                std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
            serde_json::from_str(&text).with_context(|| format!("parsing {}", p.display()))
        }
        None => Ok(replay_plan(
            info.center_hz,
            info.sample_rate_hz,
            if info.start_time.as_unix_nanos() > 0 {
                info.start_time
            } else {
                Timestamp::now()
            },
        )),
    }
}

/// A fresh data directory under the system temp dir (pid, time and a process-wide counter, so
/// concurrent callers never share one).
pub fn temp_data_dir() -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "hk-replay-{}-{}-{n}",
        std::process::id(),
        Timestamp::now().as_unix_nanos()
    ))
}

fn token(configured: Option<&str>) -> anyhow::Result<Token> {
    match configured
        .map(str::to_owned)
        .or_else(|| std::env::var("HK_TOKEN").ok())
    {
        Some(t) => Token::from_config(&t).map_err(|e| anyhow::anyhow!("API token: {e}")),
        None => Token::generate().context("generating the API token"),
    }
}

/// Starts the API server over a running pipeline.
fn serve(
    bind: SocketAddr,
    ui_dist: Option<PathBuf>,
    registry: &StreamRegistry,
    handle: &PipelineHandle,
    token: Token,
    tag: &str,
) -> anyhow::Result<Server> {
    let counters = handle.counters();
    // The pipeline writes the inventory (TrackInventory, chain record writers, plugin Ingest)
    // into this database; the API reads it through `query_inventory` only.
    let inventory = Repository::open(handle.data_dir().join("hackriff.db"))
        .context("opening the inventory database for the API")?;
    let state = ApiState {
        streams: registry.clone(),
        history: None,
        floor: Some(handle.floor_product()),
        inventory: Some(Arc::new(Mutex::new(inventory))),
        status: Some(Arc::new(move || counters.to_json())),
    };
    let mut config = ServerConfig::new(bind, token.clone());
    config.ui_dist = ui_dist;
    let server = Server::start(config, state).context("starting the HTTP server")?;
    let addr = server.local_addr();
    eprintln!("{tag}: listening on {addr}");
    if !addr.ip().is_loopback() {
        eprintln!(
            "{tag}: WARNING: bound to a non-loopback address; anyone on the network who learns \
             the token (sent in cleartext, no TLS) can read the API"
        );
    }
    let host = if addr.ip().is_unspecified() {
        SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), addr.port())
    } else {
        addr
    };
    println!("open http://{host}/#token={}", token.expose());
    Ok(server)
}

fn config_for(
    data_dir: PathBuf,
    plan: ScanPlan,
    registry: &StreamRegistry,
    feeds: Option<PathBuf>,
) -> anyhow::Result<PipelineConfig> {
    let mut cfg = PipelineConfig::new(data_dir, plan)?;
    let reg = registry.clone();
    cfg.stream_sink = Some(Arc::new(move |h, p| reg.register(h, p)));
    cfg.feeds_dir = feeds;
    Ok(cfg)
}

/// Runs `hk replay`.
pub fn run_replay(args: &ReplayArgs) -> anyhow::Result<RunSummary> {
    let pacing = if args.paced {
        Pacing::RealTime { speed: 1.0 }
    } else {
        Pacing::Unpaced
    };
    let replay = open_replay(&args.fixture, pacing, args.schedule)?;
    let plan = load_plan(args.plan.as_deref(), &replay.info)?;
    let data_dir = args.data_dir.clone().unwrap_or_else(temp_data_dir);
    let registry = StreamRegistry::new();
    let mut cfg = config_for(data_dir, plan, &registry, args.feeds.clone())?;
    cfg.source_class = replay.class;
    // Explicit opt-in: `PipelineConfig` defaults to lossless off (live-source semantics), and a
    // recording can pause, so unpaced replay waits for slow readers instead of dropping.
    cfg.lossless = !args.paced;
    cfg.drive_scheduler = args.schedule;
    if let Some(hw) = &replay.meta.global.hw {
        cfg.device_id = format!("sigmf:{hw}");
    }
    let handle = hk_pipeline::Pipeline::start(
        cfg,
        Box::new(replay.source),
        replay.info,
        None,
        Box::new(TrackInventory::default()),
    )?;
    let server = match args.serve {
        Some(bind) => Some(serve(
            bind,
            args.ui_dist.clone(),
            &registry,
            &handle,
            token(None)?,
            "hk replay",
        )?),
        None => None,
    };
    let summary = handle.wait()?;
    if server.is_some() {
        print!("{}", summary.to_text());
        eprintln!("hk replay: finished; still serving the API (Ctrl-C to stop)");
        let _server = server;
        loop {
            std::thread::park();
        }
    }
    Ok(summary)
}

/// A running daemon: the pipeline and its API server.
pub struct Daemon {
    /// The API server.
    pub server: Server,
    /// The pipeline.
    pub handle: PipelineHandle,
}

/// Starts `hackriffd`'s pipeline (scheduler driven) and API server.
pub fn start_daemon(args: &DaemonArgs) -> anyhow::Result<Daemon> {
    let Some(path) = args.source.strip_prefix("sigmf:").map(PathBuf::from) else {
        anyhow::bail!(
            "unsupported --source {:?}: use sigmf:<file.sigmf-meta> (the live HackRF source is \
             not implemented yet)",
            args.source
        );
    };
    let pacing = if args.unpaced {
        Pacing::Unpaced
    } else {
        Pacing::RealTime { speed: 1.0 }
    };
    let replay = open_replay(&path, pacing, true)?;
    let plan = load_plan(args.plan.as_deref(), &replay.info)?;
    let registry = StreamRegistry::new();
    let mut cfg = config_for(args.data_dir.clone(), plan, &registry, args.feeds.clone())?;
    cfg.source_class = replay.class;
    // Explicit opt-in, as for `hk replay` (a future live source must leave this off).
    cfg.lossless = args.unpaced;
    cfg.drive_scheduler = true;
    cfg.device_id = replay
        .meta
        .global
        .hw
        .as_ref()
        .map_or_else(|| "sigmf-replay".into(), |hw| format!("sigmf:{hw}"));
    let reopen: Option<SourceFactory> = if args.loop_replay {
        let p = path.clone();
        Some(Box::new(move || -> anyhow::Result<Box<dyn Source>> {
            Ok(Box::new(open_replay(&p, pacing, true)?.source))
        }))
    } else {
        None
    };
    let token = token(args.token.as_deref())?;
    let handle = hk_pipeline::Pipeline::start(
        cfg,
        Box::new(replay.source),
        replay.info,
        reopen,
        Box::new(TrackInventory::default()),
    )?;
    let server = serve(
        args.bind,
        args.ui_dist.clone(),
        &registry,
        &handle,
        token,
        "hackriffd",
    )?;
    Ok(Daemon { server, handle })
}

/// Runs `hackriffd` until the process is stopped.
pub fn run_daemon(args: &DaemonArgs) -> anyhow::Result<()> {
    let Daemon { server, handle } = start_daemon(args)?;
    let summary = handle.wait()?;
    print!("{}", summary.to_text());
    eprintln!("hackriffd: source finished; still serving the API (Ctrl-C to stop)");
    let _server = server;
    loop {
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    use super::*;

    /// Writes `secs` of a ci8 tone (+50 kHz) in noise at 250 kS/s, 433.5 MHz (no LFS data needed).
    fn tiny_recording(dir: &Path, secs: f64) -> PathBuf {
        use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
        std::fs::create_dir_all(dir).unwrap();
        let fs = 250e3;
        let n = (secs * fs) as usize;
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut noise = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ((state >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 12.0
        };
        let mut data = Vec::with_capacity(2 * n);
        for i in 0..n {
            let ph = 2.0 * std::f64::consts::PI * 50e3 * i as f64 / fs;
            let re = (40.0 * ph.cos() + noise()).round().clamp(-128.0, 127.0) as i8;
            let im = (40.0 * ph.sin() + noise()).round().clamp(-128.0, 127.0) as i8;
            data.push(re as u8);
            data.push(im as u8);
        }
        std::fs::write(dir.join("tiny.sigmf-data"), data).unwrap();
        let mut meta = SigmfMeta::new(Datatype::Ci8);
        meta.global.sample_rate = Some(fs);
        meta.captures.push(Capture {
            sample_start: 0,
            frequency: Some(433.5e6),
            datetime: Some("2026-09-13T12:00:00Z".into()),
            provenance: None,
            clip_count: None,
            extra: serde_json::Map::new(),
        });
        let path = dir.join("tiny.sigmf-meta");
        meta.write(&path).unwrap();
        path
    }

    /// A real HackRF fixture whose LFS data is fetched, searched from this crate up through the
    /// ancestor checkouts (a git worktree may hold only pointers). `None` skips
    /// (`HK_REQUIRE_FIXTURES=1` fails instead).
    fn lfs_fixture(name: &str) -> Option<PathBuf> {
        let rel = Path::new("fixtures/hackrf/2026-09-13").join(format!("{name}.sigmf-meta"));
        for dir in Path::new(env!("CARGO_MANIFEST_DIR")).ancestors() {
            let meta = dir.join(&rel);
            let fetched = std::fs::read(meta.with_extension("sigmf-data"))
                .is_ok_and(|b| b.len() > 4096 && !b.starts_with(b"version https://git-lfs"));
            if fetched {
                return Some(meta);
            }
        }
        if std::env::var("HK_REQUIRE_FIXTURES").is_ok_and(|v| v == "1") {
            panic!("{name}: fixture data not fetched (git lfs pull)");
        }
        eprintln!("SKIP {name}: fixture data is not fetched (git lfs pull)");
        None
    }

    /// T-027 review fix: the HackRF fixtures start at `core:global_index` 423 000 000 (915 MHz)
    /// and 324 000 000 (433 MHz), far beyond the lossless gate's slack from the always-on
    /// readers' initial cursors. Unpaced `hk replay` must finish with every sample read.
    #[test]
    fn unpaced_replay_completes_on_large_global_index_fixtures() {
        const LIMIT: Duration = Duration::from_secs(1200);
        let runs: Vec<_> = [
            "ism_915M_10M_l24g30a1_t42p3_1p2s",
            "ism_433p62M_2M_l24g30a1_t162p0_6s",
        ]
        .into_iter()
        .filter_map(|name| lfs_fixture(name).map(|f| (name, f)))
        .map(|(name, fixture)| {
            let dir = temp_data_dir();
            let (tx, rx) = std::sync::mpsc::channel();
            let args = ReplayArgs {
                fixture,
                data_dir: Some(dir.clone()),
                ..ReplayArgs::default()
            };
            // A deadlocked run leaks its thread; the timeout fails the test instead of hanging.
            std::thread::spawn(move || {
                let _ = tx.send(run_replay(&args).map_err(|e| format!("{e:#}")));
            });
            (name, dir, rx)
        })
        .collect();
        for (name, dir, rx) in runs {
            let summary = rx
                .recv_timeout(LIMIT)
                .unwrap_or_else(|_| panic!("{name}: hk replay did not finish (gate deadlock)"))
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            eprintln!("{name}\n{}", summary.to_text());
            assert!(summary.errors.is_empty(), "{name}: {:?}", summary.errors);
            let samples = summary.counter("/source/samples");
            assert_eq!(samples, 12_000_000, "{name}: every recorded sample");
            assert_eq!(summary.counter("/source/ring_errors"), 0, "{name}");
            assert_eq!(summary.always_on_lost_samples, 0, "{name}: no drops");
            for r in ["detect", "history", "spectrum"] {
                assert_eq!(
                    summary.counter(&format!("/readers/{r}/samples")),
                    samples,
                    "{name}: {r} read every sample"
                );
            }
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    fn get(addr: SocketAddr, path: &str, token: Option<&str>) -> (u16, String) {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
        write!(
            s,
            "GET {path} HTTP/1.1\r\nHost: t\r\n{auth}Connection: close\r\n\r\n"
        )
        .unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).unwrap();
        let status = raw[9..12].parse().unwrap();
        let body = raw.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_owned();
        (status, body)
    }

    #[test]
    fn replay_runs_a_small_recording_end_to_end() {
        let dir = temp_data_dir();
        let fixture = tiny_recording(&dir.join("src"), 0.25);
        let summary = run_replay(&ReplayArgs {
            fixture,
            data_dir: Some(dir.clone()),
            ..ReplayArgs::default()
        })
        .unwrap();
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        assert!(summary.counter("/source/samples") > 0);
        assert_eq!(summary.always_on_lost_samples, 0);
        assert!(summary.counter("/readers/detect/frames") > 0);
        assert!(dir.join("hackriff.db").is_file());
        let text = summary.to_text();
        assert!(text.contains("detections:"), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn daemon_drives_the_scheduler_and_serves_token_gated_status() {
        const TOKEN: &str = "t027-daemon-status-token-0123456789";
        let dir = temp_data_dir();
        let fixture = tiny_recording(&dir.join("src"), 3.0);
        let Daemon { server, handle } = start_daemon(&DaemonArgs {
            source: format!("sigmf:{}", fixture.display()),
            loop_replay: false,
            data_dir: dir.clone(),
            plan: None,
            bind: "127.0.0.1:0".parse().unwrap(),
            ui_dist: None,
            unpaced: true,
            feeds: None,
            token: Some(TOKEN.into()),
        })
        .unwrap();
        let addr = server.local_addr();
        let (unauth, _) = get(addr, "/api/status", None);
        assert_eq!(unauth, 401, "status needs the token");
        let (ok, body) = get(addr, "/api/status", Some(TOKEN));
        assert_eq!(ok, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v.pointer("/readers/detect/lost_samples").is_some(), "{v}");
        assert!(v.pointer("/chains/attached").is_some(), "{v}");
        let (_, streams) = get(addr, "/api/streams", Some(TOKEN));
        assert!(streams.contains("spectrum/live"), "{streams}");
        let summary = handle.wait().unwrap();
        eprintln!("{}", summary.to_text());
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        assert!(summary.counter("/scheduler/steps") > 10, "scheduler driven");
        assert_eq!(summary.always_on_lost_samples, 0);
        let (after, body) = get(addr, "/api/status", Some(TOKEN));
        assert_eq!(after, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v.pointer("/source/samples")
                .and_then(serde_json::Value::as_u64),
            Some(summary.counter("/source/samples"))
        );
        drop(server);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn daemon_loop_continues_the_stream_across_passes() {
        let dir = temp_data_dir();
        let fixture = tiny_recording(&dir.join("src"), 0.25);
        let Daemon { server, handle } = start_daemon(&DaemonArgs {
            source: format!("sigmf:{}", fixture.display()),
            loop_replay: true,
            data_dir: dir.clone(),
            plan: None,
            bind: "127.0.0.1:0".parse().unwrap(),
            ui_dist: None,
            unpaced: true,
            feeds: None,
            token: Some("t027-daemon-loop-token-0123456789".into()),
        })
        .unwrap();
        let counters = handle.counters();
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while counters
            .source
            .loops
            .load(std::sync::atomic::Ordering::Relaxed)
            < 2
        {
            assert!(std::time::Instant::now() < deadline, "no loop");
            std::thread::sleep(Duration::from_millis(10));
        }
        handle.stop();
        let s = handle.wait().unwrap();
        eprintln!("{}", s.to_text());
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        let loops = s.counter("/source/loops");
        assert!(loops >= 2);
        assert_eq!(
            s.counter("/source/ring_errors"),
            0,
            "indices continue across passes"
        );
        assert_eq!(s.always_on_lost_samples, 0);
        assert_eq!(
            s.counter("/readers/detect/samples"),
            s.counter("/source/samples"),
            "the detection reader read every pass"
        );
        assert!(
            s.counter("/readers/detect/stft_resets") >= loops,
            "each pass starts with a GAP reset"
        );
        drop(server);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn daemon_rejects_unsupported_sources() {
        let err = start_daemon(&DaemonArgs {
            source: "hackrf".into(),
            loop_replay: false,
            data_dir: temp_data_dir(),
            plan: None,
            bind: "127.0.0.1:0".parse().unwrap(),
            ui_dist: None,
            unpaced: true,
            feeds: None,
            token: None,
        })
        .err()
        .expect("rejected");
        assert!(err.to_string().contains("sigmf:"), "{err}");
    }
}
