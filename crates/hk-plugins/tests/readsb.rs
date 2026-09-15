//! T-015 (SIGNAL-001, ADS-B / Mode S): the readsb plugin wrapper end to end, driving the real
//! `readsb` binary (skipped with a clear message when it is not on `PATH`) through
//! `hk-plugin-readsb` and the T-014 plugin host.
//!
//! **Fixture.** A recorded 1090 MHz capture is blocked: the antenna on the dev Mac cannot hear
//! ADS-B (0 CRC-valid readsb messages when tried). This test replays the synthetic
//! `adsb_squitter` scenario instead (`py/hkpy/synth/scenarios.py`; 4 ICAO addresses x 4 DF17
//! squitters each = 16 messages, CRC-24 valid by construction). TODO: swap in a recorded 1090 MHz
//! SigMF fixture once one exists (docs/stream-contract.md §9.6).
//!
//! See `docs/adr/0010-language-and-licence-ledger.md` for readsb's licence (GPL-3.0-or-later) and
//! why it only ever runs as a subprocess.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::sigmf::Datatype;
use hk_model::{
    ContentClass, CrcStatus, DecodedIdentity, Identity, IdentityScheme, Repository, SampleTime,
    Timestamp,
};
use hk_plugins::{
    HostError, Ingest, InputStreamDesc, ManifestError, PluginContext, PluginInstance,
    PluginManifest, PluginMonitor, PluginState, RestartPolicy,
};
use hk_stream::{
    BinaryRecord, Listener, Publisher, PublisherConfig, Record, RecordFlags, StreamHeader,
    StreamKind, StreamReader,
};

/// Serialises every test in this file that spawns a `PluginInstance` (i.e. calls
/// `Command::spawn` for the wrapper) against the two tests that temporarily override the
/// `HK_READSB`/`FAKE_READSB_*` process environment: `std::env::set_var` (`unsafe` since Rust
/// 2024, precisely because concurrent reads/spawns can observe a torn value) must not race a
/// spawn that expects the unmodified environment.
static ENV_MUTEX: Mutex<()> = Mutex::new(());

const WRAPPER: &str = env!("CARGO_BIN_EXE_hk-plugin-readsb");
/// A stand-in for readsb (see its module doc), used only by the crash/idle-handling tests below
/// so they don't need a real 9s readsb watchdog wait or shell-timing tricks.
const FAKE_READSB: &str = env!("CARGO_BIN_EXE_hk-fake-readsb");
const CENTER_HZ: f64 = 1_090_000_000.0;
const RATE: f64 = 2_400_000.0;
const CHUNK_SAMPLES: usize = 4096;
const SIGNAL_001: &str = "SIGNAL-001";

/// Whether `readsb` is installed on this machine. Honours `HK_READSB`, the same override the
/// wrapper itself reads, so a caller pinning a specific build doesn't get skipped.
fn readsb_available() -> bool {
    if let Ok(path) = std::env::var("HK_READSB") {
        return Path::new(&path).is_file();
    }
    Command::new("readsb")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// Prints a skip message and returns from the enclosing test when `readsb` is not installed
/// (mirrors `hk_e2e::synth_or_skip!` for the Python generator's `uv` dependency).
macro_rules! readsb_or_skip {
    () => {
        if !readsb_available() {
            eprintln!(
                "SKIP {}: readsb not found on PATH (install it, e.g. `brew install readsb`, to run this test)",
                module_path!()
            );
            return;
        }
    };
}

/// The repository's readsb manifest, pointed at the freshly built wrapper binary.
fn manifest() -> PluginManifest {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/readsb/manifest.json");
    let mut m = PluginManifest::load(path).unwrap();
    m.executable = WRAPPER.into();
    m
}

fn input(anchor: SampleTime) -> InputStreamDesc {
    InputStreamDesc {
        datatype: Datatype::Ci8,
        sample_rate_hz: RATE,
        center_hz: Some(CENTER_HZ),
        bandwidth_hz: Some(2.4e6),
        content_class: ContentClass::Unrestricted,
        anchor,
        emitter_id: None,
        provenance_ref: None,
    }
}

fn anchor() -> SampleTime {
    SampleTime {
        sample_index: 0,
        host_time: Timestamp::now(),
    }
}

/// A short fresh directory (Unix socket paths are limited to ~104 bytes on macOS).
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hkr{tag}{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait_running(inst: &PluginInstance) {
    let mon = inst.monitor();
    assert!(
        mon.wait_for(Duration::from_secs(15), |s| s.state == PluginState::Running),
        "{:?} {:?}",
        mon.stats(),
        mon.log_tail()
    );
}

fn wait(mon: &PluginMonitor, what: &str, pred: impl FnMut(&hk_plugins::PluginStats) -> bool) {
    assert!(
        mon.wait_for(Duration::from_secs(30), pred),
        "timed out waiting for {what}: {:?} {:?}",
        mon.stats(),
        mon.log_tail()
    );
}

/// Feeds raw `ci8` bytes to the plugin in fixed-size chunks starting at `start_sample`; returns
/// the sample index just past the last chunk.
fn push_all(inst: &mut PluginInstance, bytes: &[u8], start_sample: u64) -> u64 {
    let bps = Datatype::Ci8.bytes_per_sample();
    let mut sample = start_sample;
    for chunk in bytes.chunks(CHUNK_SAMPLES * bps) {
        let n = chunk.len() as u64 / bps as u64;
        inst.push(BinaryRecord {
            t: Timestamp::now(),
            sample_index: sample,
            flags: RecordFlags::empty(),
            payload: chunk,
        })
        .unwrap();
        sample += n;
    }
    sample
}

/// readsb's `ifile` reader only hands a block to the decoder once it has read a *full*
/// `--sdr-buffer-size` block (default 128 KiB, ~65536 UC8 samples) or hit EOF (`sdr_ifile.c`
/// `ifileRun`). This test never closes readsb's stdin (a live wideband channel has no EOF), so
/// without padding the last, partially-filled block — which can hold the fixture's final
/// messages — never gets dispatched, and readsb's own watchdog eventually declares itself
/// "wedged" for want of new data. Trailing silence (`ci8` zero, i.e. `uc8` mid-scale after the
/// wrapper's XOR conversion) pushes the tail block over that threshold; two full blocks' worth
/// is ample margin over the ~44 KiB the default block size needs to complete. Real traffic makes
/// this a non-issue: a live channel keeps streaming samples regardless of message content.
const TRAIL_PAD_SAMPLES: usize = 2 * 65536;

/// [`push_all`], followed by enough trailing silence to flush readsb's last, partially-filled
/// `ifile` read block (see [`TRAIL_PAD_SAMPLES`]).
fn push_replay(inst: &mut PluginInstance, bytes: &[u8], start_sample: u64) -> u64 {
    let sample = push_all(inst, bytes, start_sample);
    let silence = vec![0u8; TRAIL_PAD_SAMPLES * Datatype::Ci8.bytes_per_sample()];
    push_all(inst, &silence, sample)
}

/// Request for the shared `adsb_squitter` scenario (4 ICAO addresses x 4 DF17 squitters each).
fn adsb_request(seed: u64) -> SynthRequest {
    SynthRequest::new("adsb_squitter")
        .seed(seed)
        .param("duration_s", 0.1)
        .param("messages_per_aircraft", 4)
}

/// T-223: the wrapper reports ready (the contract's `ready` line, §9.3) only once readsb's Beast
/// connection is up and its pre-roll is written — everything it needs to give a message its own
/// sample time. A chain that holds its first record until then never feeds a squitter that would
/// decode before the connection exists and keep only the fallback stamp (under load the
/// connection took 1.6 s, and the first squitter was stamped 158 ms late).
///
/// Nothing is pushed here: the wrapper feeds readsb its own silence while it waits, so readiness
/// never depends on the chain's input — which is what lets the chain wait for it.
#[test]
fn ready_is_reported_only_after_readsbs_beast_connection_is_up() {
    readsb_or_skip!();
    let _guard = ENV_MUTEX.lock().unwrap();
    let ingest = Arc::new(Mutex::new(Ingest::new(
        Repository::open_in_memory().unwrap(),
    )));
    let inst = PluginInstance::spawn(
        manifest(),
        input(anchor()),
        PluginContext::default(),
        ingest,
    )
    .unwrap();
    wait_running(&inst);
    assert!(
        inst.wait_ready(Duration::from_secs(60)),
        "{:?} {:?}",
        inst.stats(),
        inst.monitor().log_tail()
    );
    let stats = inst.stats();
    assert_eq!(
        stats.records_offered_before_ready, 0,
        "nothing was pushed: {stats:?}"
    );
    let tail = inst.monitor().log_tail();
    assert!(
        tail.lines.iter().any(|l| l.contains("Beast connected")),
        "ready was reported before readsb's Beast connection: {tail:?}"
    );
    inst.shutdown();
}

fn assert_gone(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    // SAFETY: signal 0 only checks for existence.
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(Instant::now() < deadline, "readsb pid {pid} still alive");
        thread::sleep(Duration::from_millis(10));
    }
}

/// The most recent `readsb pid <N>` line the wrapper has logged.
fn readsb_pid(mon: &PluginMonitor) -> i32 {
    mon.log_tail()
        .lines
        .iter()
        .rev()
        .find_map(|l| l.strip_prefix("hk-plugin-readsb: readsb pid "))
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or_else(|| panic!("no readsb pid logged: {:?}", mon.log_tail()))
}

/// SIGNAL-001: replaying the synthetic `adsb_squitter` fixture through the real readsb plugin
/// lands 16 CRC-valid Decode rows, upserts one Emitter per ICAO with `last_seen` updated, and
/// republishes every decode on a T-016 messages stream to an external consumer.
#[test]
fn signal_001_readsb_decodes_land_as_emitters_and_republish() {
    readsb_or_skip!();
    let _guard = ENV_MUTEX.lock().unwrap();
    let out = synth_or_skip!(adsb_request(7));
    let fx = out.fixture(0).unwrap();
    assert_eq!(fx.sample_rate, RATE);
    let truth = fx.of_kind("adsb-df17");
    assert_eq!(truth.len(), 16, "[{SIGNAL_001}] truth fixture");
    let icaos: BTreeSet<String> = truth
        .iter()
        .map(|t| t.identity().expect("icao identity").1.to_owned())
        .collect();
    assert_eq!(icaos.len(), 4, "[{SIGNAL_001}] four aircraft");
    let bytes = std::fs::read(fx.data_path()).unwrap();

    let dir = temp_dir("s1");
    let path = dir.join("adsb.sock");
    let mut header = StreamHeader::new(
        "decodes/adsb",
        StreamKind::Messages,
        ContentClass::Unrestricted,
        "hk-plugins:readsb@0.1.0",
    );
    header.message_schema = Some("hackriff.decode/1".into());
    header.max_frame_len = 64 * 1024;
    let publisher = Publisher::new(
        header,
        PublisherConfig {
            queue_bytes: 4 * 1024 * 1024,
            ..PublisherConfig::default()
        },
    )
    .unwrap();
    let handle = publisher.handle();
    let listener = Listener::bind_uds(&path, handle.clone()).unwrap();
    let consumer_path = path.clone();
    let consumer = thread::spawn(move || {
        let mut reader = StreamReader::connect_uds(&consumer_path).unwrap();
        let header = reader.read_header().unwrap().clone();
        let mut records = Vec::new();
        while let Some(r) = reader.next_record().unwrap() {
            records.push(r);
        }
        (header, records)
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while handle.open_consumers() == 0 {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }

    let sink = Arc::new(Mutex::new(Ingest::with_republish(
        Repository::open_in_memory().unwrap(),
        publisher,
    )));
    let mut inst = PluginInstance::spawn(
        manifest(),
        input(anchor()),
        PluginContext::default(),
        Arc::clone(&sink),
    )
    .unwrap();
    wait_running(&inst);
    push_replay(&mut inst, &bytes, 0);
    let mon = inst.monitor();
    wait(&mon, "16 decodes", |s| s.decodes >= 16);
    let final_stats = mon.stats();
    drop(mon);
    inst.shutdown();

    let mut ingest = Arc::try_unwrap(sink)
        .ok()
        .expect("host released the ingest")
        .into_inner()
        .unwrap();
    drop(ingest.take_publisher());
    let (stream_header, records) = consumer.join().unwrap();
    drop(listener);

    assert_eq!(final_stats.decodes, 16, "[{SIGNAL_001}]");
    assert_eq!(final_stats.malformed, 0, "{final_stats:?}");
    assert_eq!(
        ingest.stats().emitters_upserted,
        16,
        "[{SIGNAL_001}] one upsert per decode"
    );

    for icao in &icaos {
        let identity = DecodedIdentity {
            scheme: IdentityScheme::AdsbIcao,
            value: icao.clone(),
        };
        let decodes = ingest.repo().decodes_for_identity(&identity).unwrap();
        assert!(!decodes.is_empty(), "[{SIGNAL_001}] {icao}");
        assert!(
            decodes.iter().all(|d| d.crc_status == CrcStatus::Valid),
            "[{SIGNAL_001}] {icao}: {decodes:?}"
        );
        let emitter = ingest
            .repo()
            .emitter_by_identity(&identity)
            .unwrap()
            .unwrap_or_else(|| panic!("[{SIGNAL_001}] no emitter for {icao}"));
        assert_eq!(emitter.identity, Identity::Decoded(identity));
        let max_t = decodes.iter().map(|d| d.t).max().unwrap();
        assert_eq!(
            emitter.last_seen, max_t,
            "[{SIGNAL_001}] {icao}: last_seen tracks the latest decode"
        );
        assert_eq!(emitter.count, decodes.len() as u64);

        // Truth-checked fields, not just presence: callsign, resolved CPR position and velocity.
        let truth_for_icao = truth
            .iter()
            .filter(|t| t.identity() == Some(("icao", icao.as_str())))
            .collect::<Vec<_>>();
        let expected_callsign = truth_for_icao
            .iter()
            .find(|t| t.str("message_kind") == Some("identification"))
            .and_then(|t| t.str("/metadata/callsign"))
            .unwrap_or_else(|| panic!("[{SIGNAL_001}] {icao}: no identification truth"));
        let decoded_callsign = decodes
            .iter()
            .find_map(|d| d.metadata.get("callsign").and_then(|v| v.as_str()));
        assert_eq!(
            decoded_callsign,
            Some(expected_callsign),
            "[{SIGNAL_001}] {icao} callsign"
        );

        if let Some(tp) = truth_for_icao
            .iter()
            .find(|t| t.str("message_kind") == Some("position-even"))
        {
            let (exp_lat, exp_lon) = (
                tp.f64("/metadata/lat").unwrap(),
                tp.f64("/metadata/lon").unwrap(),
            );
            let (lat, lon) = decodes
                .iter()
                .find_map(|d| {
                    Some((
                        d.metadata.get("lat")?.as_f64()?,
                        d.metadata.get("lon")?.as_f64()?,
                    ))
                })
                .unwrap_or_else(|| panic!("[{SIGNAL_001}] {icao}: no decoded position"));
            assert!(
                (lat - exp_lat).abs() < 0.01,
                "[{SIGNAL_001}] {icao} lat {lat} vs truth {exp_lat}"
            );
            assert!(
                (lon - exp_lon).abs() < 0.01,
                "[{SIGNAL_001}] {icao} lon {lon} vs truth {exp_lon}"
            );
        }

        if let Some(tv) = truth_for_icao
            .iter()
            .find(|t| t.str("message_kind") == Some("velocity"))
        {
            let (exp_ew, exp_ns, exp_vr) = (
                tv.f64("/metadata/ew_velocity_kt").unwrap(),
                tv.f64("/metadata/ns_velocity_kt").unwrap(),
                tv.f64("/metadata/vertical_rate_fpm").unwrap(),
            );
            let (ew, ns, vr) = decodes
                .iter()
                .find_map(|d| {
                    Some((
                        d.metadata.get("ew_velocity_kt")?.as_f64()?,
                        d.metadata.get("ns_velocity_kt")?.as_f64()?,
                        d.metadata.get("vertical_rate_fpm")?.as_f64()?,
                    ))
                })
                .unwrap_or_else(|| panic!("[{SIGNAL_001}] {icao}: no decoded velocity"));
            assert_eq!(ew, exp_ew, "[{SIGNAL_001}] {icao} ew_velocity_kt");
            assert_eq!(ns, exp_ns, "[{SIGNAL_001}] {icao} ns_velocity_kt");
            assert_eq!(vr, exp_vr, "[{SIGNAL_001}] {icao} vertical_rate_fpm");
        }
    }
    let total_decodes: usize = icaos
        .iter()
        .map(|icao| {
            ingest
                .repo()
                .decodes_for_identity(&DecodedIdentity {
                    scheme: IdentityScheme::AdsbIcao,
                    value: icao.clone(),
                })
                .unwrap()
                .len()
        })
        .sum();
    assert_eq!(total_decodes, 16, "[{SIGNAL_001}]");

    assert_eq!(stream_header.kind, StreamKind::Messages);
    assert_eq!(stream_header.content_class, ContentClass::Unrestricted);
    assert_eq!(records.len(), 16, "[{SIGNAL_001}] every decode republished");
    for r in &records {
        let Record::Message(msg) = r else {
            panic!("[{SIGNAL_001}] {r:?}")
        };
        assert_eq!(msg.content_class, ContentClass::Unrestricted);
        assert_eq!(msg.value["decoder"], "readsb@0.1.0");
        assert_eq!(msg.value["crc_status"], "valid");
        let icao = msg.value["identity"]["value"].as_str().unwrap();
        assert!(icaos.contains(icao), "[{SIGNAL_001}] {icao}");
        assert_eq!(msg.value["identity"]["scheme"], "adsb-icao");
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// SIGNAL-001 fault injection: killing the readsb child mid-stream is isolated to one plugin
/// crash. The host restarts the wrapper (and with it a fresh readsb), the producer never blocks,
/// no zombie or grandchild is left, and decoding resumes.
#[test]
fn readsb_child_crash_mid_stream_is_isolated_and_recovers() {
    readsb_or_skip!();
    let _guard = ENV_MUTEX.lock().unwrap();
    let out = synth_or_skip!(adsb_request(3));
    let fx = out.fixture(0).unwrap();
    let bytes = std::fs::read(fx.data_path()).unwrap();

    let mut m = manifest();
    m.restart = RestartPolicy {
        backoff_initial: Duration::from_millis(20),
        backoff_max: Duration::from_millis(200),
        max_restarts: 5,
        window: Duration::from_secs(60),
    };
    let sink = Arc::new(Mutex::new(Ingest::new(
        Repository::open_in_memory().unwrap(),
    )));
    let mut inst = PluginInstance::spawn(
        m,
        input(anchor()),
        PluginContext::default(),
        Arc::clone(&sink),
    )
    .unwrap();
    wait_running(&inst);
    push_replay(&mut inst, &bytes, 0);
    let mon = inst.monitor();
    wait(&mon, "first pass decodes", |s| s.decodes >= 16);
    let before = mon.stats();

    let pid = readsb_pid(&mon);
    // SAFETY: plain kill(2) of a pid this test just found in the plugin's own log.
    unsafe {
        assert_eq!(libc::kill(pid, libc::SIGKILL), 0, "kill readsb pid {pid}");
    }

    // The wrapper only notices readsb is gone on its next write to it (a broken pipe): keep
    // pushing small chunks until that happens. `kill()` only *enqueues* SIGKILL — this process
    // exercising the actual OS mechanics rather than waiting a fixed guess for the write to land
    // after teardown finishes (empirically: a write immediately after `kill()` routinely still
    // succeeds; readsb's fds are not torn down synchronously with the signal). Never blocks the
    // producer for more than one chunk's enqueue at a time either way.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut worst = Duration::ZERO;
    let mut sample = 1_000_000u64;
    while mon.stats().crashes == before.crashes {
        assert!(
            Instant::now() < deadline,
            "readsb's death was never detected: {:?} {:?}",
            mon.stats(),
            mon.log_tail()
        );
        let t0 = Instant::now();
        sample = push_all(&mut inst, &bytes[..CHUNK_SAMPLES * 2], sample);
        worst = worst.max(t0.elapsed());
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        worst < Duration::from_millis(200),
        "push blocked: {worst:?}"
    );
    assert_gone(pid);
    wait_running(&inst); // a fresh wrapper + readsb pair has come up
    // A fresh process was spawned (`starts` counts every spawn, not just this one); pids on a
    // busy machine can be reused quickly, so this — not pid inequality — is the reliable check.
    assert!(
        mon.stats().starts >= 2,
        "expected a second spawn: {:?}",
        mon.stats()
    );
    let new_pid = readsb_pid(&mon);

    // Recovery: a full second pass through the restarted instance decodes all 16 again.
    push_replay(&mut inst, &bytes, 2_000_000);
    wait(&mon, "second pass decodes", |s| {
        s.decodes >= before.decodes + 16
    });
    let after = mon.stats();
    drop(mon);
    let final_stats = inst.shutdown();
    assert_eq!(final_stats.state, PluginState::Stopped);
    assert_gone(new_pid);
    println!(
        "readsb crash mid-stream: {} starts, {} crashes, {} restarts, {} decodes total; worst push while dead {worst:?}",
        after.starts, after.crashes, after.restarts, after.decodes
    );

    let sink = sink.lock().unwrap();
    assert!(
        sink.repo()
            .emitter_by_identity(&DecodedIdentity {
                scheme: IdentityScheme::AdsbIcao,
                value: "a0b1c2".into(),
            })
            .unwrap()
            .is_some()
    );
}

/// The wrapper reacts to readsb's own "SDR wedged" self-destruct message the instant the stderr
/// pump sees it, without waiting for the (possibly slow-to-actually-exit) child. Uses
/// [`FAKE_READSB`] in `wedge_immediately` mode (module doc), which prints the line and then
/// sleeps far longer than this test's timeout — so a fast crash here proves the stderr-detection
/// path fired, not the waiter thread blocked on the child's own exit. Does not need readsb
/// installed.
#[test]
fn readsb_wedge_message_ends_the_wrapper_without_waiting_for_the_child() {
    let _guard = ENV_MUTEX.lock().unwrap();
    // SAFETY: serialised by ENV_MUTEX against every other test in this file that spawns a
    // `PluginInstance` or touches these variables.
    unsafe {
        std::env::set_var("HK_READSB", FAKE_READSB);
        std::env::set_var("FAKE_READSB_MODE", "wedge_immediately");
    }
    let mut m = manifest();
    m.restart = RestartPolicy {
        backoff_initial: Duration::from_millis(20),
        backoff_max: Duration::from_millis(200),
        max_restarts: 5,
        window: Duration::from_secs(60),
    };
    let sink = Arc::new(Mutex::new(Ingest::new(
        Repository::open_in_memory().unwrap(),
    )));
    let t0 = Instant::now();
    let inst = PluginInstance::spawn(
        m,
        input(anchor()),
        PluginContext::default(),
        Arc::clone(&sink),
    )
    .unwrap();
    let mon = inst.monitor();
    wait(&mon, "a crash from the wedge message", |s| s.crashes >= 1);
    let elapsed = t0.elapsed();
    drop(mon);
    let stats = inst.shutdown();
    // SAFETY: same as above.
    unsafe {
        std::env::remove_var("HK_READSB");
        std::env::remove_var("FAKE_READSB_MODE");
    }
    assert!(stats.crashes >= 1, "{stats:?}");
    assert!(
        elapsed < Duration::from_secs(10),
        "took {elapsed:?}: looks like the wrapper waited for the child (60s sleep) \
         instead of reacting to the wedge line in its stderr"
    );
}

/// The wrapper's keepalive (a small silence chunk whenever it goes `KEEPALIVE_INTERVAL` without
/// real data to forward) keeps readsb from ever seeing enough idle time to hit its own stall
/// watchdog. Verified against [`FAKE_READSB`] in `idle_wedge` mode with a test-friendly 5s
/// threshold (real readsb's is ~9s): an 8s gap with no pushes must not cross it. Does not need
/// readsb installed.
#[test]
fn keepalive_prevents_a_false_wedge_during_an_idle_gap() {
    let _guard = ENV_MUTEX.lock().unwrap();
    // SAFETY: serialised by ENV_MUTEX against every other test in this file.
    unsafe {
        std::env::set_var("HK_READSB", FAKE_READSB);
        std::env::set_var("FAKE_READSB_MODE", "idle_wedge");
        std::env::set_var("FAKE_READSB_IDLE_MS", "5000");
    }
    let sink = Arc::new(Mutex::new(Ingest::new(
        Repository::open_in_memory().unwrap(),
    )));
    let mut inst = PluginInstance::spawn(
        manifest(),
        input(anchor()),
        PluginContext::default(),
        Arc::clone(&sink),
    )
    .unwrap();
    wait_running(&inst);
    push_all(&mut inst, &[0x00; 4096], 0); // a little real data, then nothing for a while

    thread::sleep(Duration::from_secs(8)); // longer than the fake's 5s idle threshold

    let stats = inst.shutdown();
    // SAFETY: same as above.
    unsafe {
        std::env::remove_var("HK_READSB");
        std::env::remove_var("FAKE_READSB_MODE");
        std::env::remove_var("FAKE_READSB_IDLE_MS");
    }
    assert_eq!(stats.crashes, 0, "{stats:?}");
    assert_eq!(stats.state, PluginState::Stopped);
}

/// T-103: a readsb whose Beast connection comes up late (over the old 2 s connect wait, as under
/// heavy load) still gets the chain's first squitter stamped exactly. [`FAKE_READSB`] in
/// `late_beast` mode connects after 3 s and "decodes" one DF17 at the first non-silence sample;
/// the wrapper must hold real samples until the connection exists. Before the fix it forwarded
/// them after 2 s, the frame never reached Beast, and the decode kept the fallback stamp (the
/// newest record's last sample), outside the acceptance suite's 2 ms tolerance. Also checks the
/// host's `finish`: every queued record is delivered and the pair exits cleanly. Does not need
/// readsb installed.
#[test]
fn first_squitter_is_stamped_exactly_when_readsbs_beast_connection_is_slow() {
    let _guard = ENV_MUTEX.lock().unwrap();
    // SAFETY: serialised by ENV_MUTEX against every other test in this file.
    unsafe {
        std::env::set_var("HK_READSB", FAKE_READSB);
        std::env::set_var("FAKE_READSB_MODE", "late_beast");
        std::env::set_var("FAKE_READSB_CONNECT_DELAY_MS", "3000");
    }
    let a = anchor();
    let sink = Arc::new(Mutex::new(Ingest::new(
        Repository::open_in_memory().unwrap(),
    )));
    let mut inst = PluginInstance::spawn(
        manifest(),
        input(a),
        PluginContext::default(),
        Arc::clone(&sink),
    )
    .unwrap();
    wait_running(&inst);
    // Silence, then a record whose sample 100 is the squitter (the fake's first non-silence
    // sample), then silence again.
    let start = 5_000_000u64;
    let mut next = push_all(&mut inst, &vec![0u8; 8 * CHUNK_SAMPLES * 2], start);
    let mut marked = vec![0u8; CHUNK_SAMPLES * 2];
    marked[2 * 100] = 0x40;
    let squitter_index = next + 100;
    next = push_all(&mut inst, &marked, next);
    push_all(&mut inst, &vec![0u8; 8 * CHUNK_SAMPLES * 2], next);
    let stats = inst.finish(Duration::from_secs(30));
    // SAFETY: same as above.
    unsafe {
        std::env::remove_var("HK_READSB");
        std::env::remove_var("FAKE_READSB_MODE");
        std::env::remove_var("FAKE_READSB_CONNECT_DELAY_MS");
    }
    assert_eq!(
        stats.records_dropped_full + stats.records_dropped_detached,
        0
    );
    assert_eq!(
        (stats.crashes, stats.restarts, stats.decodes),
        (0, 0, 1),
        "{stats:?}"
    );
    let sink = sink.lock().unwrap();
    let decodes = sink
        .repo()
        .decodes_for_identity(&DecodedIdentity {
            scheme: IdentityScheme::AdsbIcao,
            value: "4840d6".into(),
        })
        .unwrap();
    assert_eq!(decodes.len(), 1, "{stats:?}");
    assert_eq!(
        decodes[0].t,
        a.time_of(squitter_index, RATE),
        "the squitter keeps its Beast sample stamp (index {squitter_index}), not the fallback"
    );
}

/// Inputs the manifest does not accept (wrong sample rate, wrong datatype) are refused before any
/// process is spawned. Does not need readsb installed.
#[test]
fn readsb_manifest_rejects_inputs_it_does_not_accept() {
    let sink = || {
        Arc::new(Mutex::new(Ingest::new(
            Repository::open_in_memory().unwrap(),
        )))
    };

    let mut wrong_rate = input(anchor());
    wrong_rate.sample_rate_hz = 2_000_000.0;
    let err = PluginInstance::spawn(manifest(), wrong_rate, PluginContext::default(), sink())
        .err()
        .expect("wrong rate refused");
    assert!(matches!(
        err,
        HostError::Manifest(ManifestError::Mismatch(_))
    ));

    let mut wrong_datatype = input(anchor());
    wrong_datatype.datatype = Datatype::Cu8;
    let err = PluginInstance::spawn(manifest(), wrong_datatype, PluginContext::default(), sink())
        .err()
        .expect("wrong datatype refused");
    assert!(matches!(
        err,
        HostError::Manifest(ManifestError::Mismatch(_))
    ));

    let mut no_center = input(anchor());
    no_center.center_hz = None;
    let err = PluginInstance::spawn(manifest(), no_center, PluginContext::default(), sink())
        .err()
        .expect("missing centre refused");
    assert!(matches!(
        err,
        HostError::Manifest(ManifestError::Mismatch(_))
    ));
}
