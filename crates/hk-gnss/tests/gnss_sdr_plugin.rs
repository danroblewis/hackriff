//! T-323: GNSS-SDR as a C22 plugin, driven end to end through the real plugin host
//! (`hk_plugins::PluginInstance` → `hk-plugin-gnss-sdr` → a `gnss-sdr` executable → RINEX/NMEA →
//! `decode` lines → `Ingest` → republished messages → back into `hk_gnss` types).
//!
//! **What stands in for GNSS-SDR.** The dev machine has no GNSS-SDR (no Homebrew formula), so
//! these tests point the wrapper at `hk-fake-gnss-sdr`, which checks the generated config and
//! writes spec-shaped RINEX 3.02 / NMEA for a canned constellation (see its module doc). What
//! is tested is therefore the *seam* — config generation, dwell buffering, process supervision,
//! parsing, the evidence schema and what the host does with it — **not GNSS-SDR's tracking
//! accuracy**, which needs a real L1 capture from an active antenna (user-triggered).
//! `real_gnss_sdr_accepts_the_generated_config` runs the real binary when one is on `PATH`.
//!
//! Use cases: SIGNAL-030 (track and fix), AWARE-002 (real C/N0 instead of the power proxy),
//! PROP-033 (S4 from tracked C/N0).

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hk_gnss::receiver::{
    DwellSummary, FRAME_DWELL, FRAME_EPOCH, GNSS_SDR_SCHEMA, RunOutcome, geodetic_to_ecef,
};
use hk_gnss::{
    GnssObservableEpoch, IntegrityConfig, JammingVerdict, L1_HZ, PowerEvidence, assess_jamming,
    epochs_from_evidence, lock_evidence, s4_by_prn,
};
use hk_model::sigmf::Datatype;
use hk_model::{ContentClass, Repository, SampleTime, Timestamp};
use hk_plugins::{Ingest, InputStreamDesc, PluginContext, PluginInstance, PluginManifest};
use hk_stream::{
    BinaryRecord, Listener, Publisher, PublisherConfig, Record, RecordFlags, StreamHeader,
    StreamKind, StreamReader,
};
use serde_json::Value;

const WRAPPER: &str = env!("CARGO_BIN_EXE_hk-plugin-gnss-sdr");
const FAKE: &str = env!("CARGO_BIN_EXE_hk-fake-gnss-sdr");
/// 2.5 Msps, 2 s dwell: 10 MB of `ci8`, inside the manifest's 16 MiB input queue, so nothing is
/// dropped while the wrapper spools it.
const RATE: f64 = 2_500_000.0;
const DWELL_S: f64 = 2.0;
const CHUNK_SAMPLES: usize = 65_536;

/// The repository manifest, pointed at the built wrapper and a chosen `gnss-sdr`.
fn manifest(gnss_sdr: &str, extra: &[&str]) -> PluginManifest {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/gnss-sdr/manifest.json");
    let mut m = PluginManifest::load(path).unwrap();
    m.executable = WRAPPER.into();
    m.params.insert("gnss_sdr".into(), gnss_sdr.into());
    m.params.insert("dwell_s".into(), DWELL_S.to_string());
    m.params.insert("min_dwell_s".into(), "1".into());
    m.args.extend(extra.iter().map(|s| s.to_string()));
    m
}

fn input(center_hz: f64) -> InputStreamDesc {
    InputStreamDesc {
        datatype: Datatype::Ci8,
        sample_rate_hz: RATE,
        center_hz: Some(center_hz),
        bandwidth_hz: Some(RATE),
        content_class: ContentClass::Unrestricted,
        anchor: SampleTime {
            sample_index: 0,
            host_time: Timestamp::now(),
        },
        emitter_id: None,
        provenance_ref: None,
    }
}

/// Deterministic noise-like `ci8` bytes.
fn noise(samples: usize) -> Vec<u8> {
    let mut x = 0x2545_f491_u32;
    (0..samples * 2)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 24) as u8 & 0x1f
        })
        .collect()
}

struct Run {
    messages: Vec<Value>,
    emitters_upserted: u64,
    new_emitters: usize,
    annotations: u64,
    crashes: u64,
    log: Vec<String>,
}

/// Feeds one dwell of IQ through the host to the wrapper, ends the input, and returns every
/// republished message.
fn run(gnss_sdr: &str, extra: &[&str], tag: &str) -> Run {
    let dir = std::env::temp_dir().join(format!("hkg{tag}{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("m.sock");
    let mut header = StreamHeader::new(
        "decodes/gnss",
        StreamKind::Messages,
        ContentClass::Unrestricted,
        "hk-plugins:gnss-sdr@0.1.0",
    );
    header.message_schema = Some(GNSS_SDR_SCHEMA.into());
    header.max_frame_len = 1024 * 1024;
    let publisher = Publisher::new(
        header,
        PublisherConfig {
            queue_bytes: 16 * 1024 * 1024,
            ..PublisherConfig::default()
        },
    )
    .unwrap();
    let handle = publisher.handle();
    let listener = Listener::bind_uds(&sock, handle.clone()).unwrap();
    let consumer_sock = sock.clone();
    let consumer = thread::spawn(move || {
        let mut reader = StreamReader::connect_uds(&consumer_sock).unwrap();
        reader.read_header().unwrap();
        let mut out = Vec::new();
        while let Some(r) = reader.next_record().unwrap() {
            if let Record::Message(m) = r {
                out.push(m.value);
            }
        }
        out
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while handle.open_consumers() == 0 {
        assert!(Instant::now() < deadline, "consumer never connected");
        thread::sleep(Duration::from_millis(1));
    }

    let sink = Arc::new(Mutex::new(Ingest::with_republish(
        Repository::open_in_memory().unwrap(),
        publisher,
    )));
    let mut inst = PluginInstance::spawn(
        manifest(gnss_sdr, extra),
        input(L1_HZ),
        PluginContext::default(),
        Arc::clone(&sink),
    )
    .unwrap();
    assert!(
        inst.wait_ready(Duration::from_secs(90)),
        "{:?} {:?}",
        inst.stats(),
        inst.monitor().log_tail()
    );
    let bytes = noise((DWELL_S * RATE) as usize);
    let mut sample = 0u64;
    for chunk in bytes.chunks(CHUNK_SAMPLES * 2) {
        inst.push(BinaryRecord {
            t: Timestamp::now(),
            sample_index: sample,
            flags: RecordFlags::empty(),
            payload: chunk,
        })
        .unwrap();
        sample += chunk.len() as u64 / 2;
    }
    let log_mon = inst.monitor();
    let stats = inst.finish(Duration::from_secs(60));
    let log = log_mon.log_tail().lines;
    // The monitor shares the host's state, including its handle on the ingest.
    drop(log_mon);
    assert_eq!(
        stats.records_dropped_full, 0,
        "the dwell must reach the wrapper whole: {stats:?}"
    );

    let mut ingest = Arc::try_unwrap(sink)
        .ok()
        .expect("host released the ingest")
        .into_inner()
        .unwrap();
    drop(ingest.take_publisher());
    let messages = consumer.join().unwrap();
    drop(listener);
    let _ = std::fs::remove_dir_all(&dir);
    Run {
        messages,
        emitters_upserted: ingest.stats().emitters_upserted,
        new_emitters: ingest.take_new_emitters().len(),
        annotations: stats.annotations,
        crashes: stats.crashes,
        log,
    }
}

impl Run {
    fn of(&self, frame_model: &str) -> Vec<&Value> {
        self.messages
            .iter()
            .filter(|m| m["frame_model"] == frame_model)
            .collect()
    }

    fn summary(&self) -> DwellSummary {
        let s = self.of(FRAME_DWELL);
        assert_eq!(s.len(), 1, "one summary per dwell: {:?}", self.log);
        serde_json::from_value(s[0]["metadata"].clone()).unwrap()
    }

    fn epochs(&self) -> Vec<GnssObservableEpoch> {
        epochs_from_evidence(self.of(FRAME_EPOCH).into_iter().map(|m| &m["metadata"]))
    }

    /// The receiver's output is evidence about a dwell, never inventory: no identity (which
    /// would upsert an Emitter per PRN), no annotation (which targets a detection).
    fn assert_evidence_only(&self) {
        assert_eq!(
            self.emitters_upserted, 0,
            "a PRN must never become an Emitter"
        );
        assert_eq!(self.new_emitters, 0);
        assert_eq!(
            self.annotations, 0,
            "the receiver must not annotate detections"
        );
        for m in &self.messages {
            assert!(
                m.get("identity").is_none_or(Value::is_null),
                "identity on a GNSS decode: {m}"
            );
            assert!(m.get("annotation_id").is_none_or(Value::is_null));
        }
    }
}

/// SIGNAL-030: the track-and-fix half — tracked satellites with pseudorange and carrier phase,
/// a position fix, and GPS time converted to UTC.
#[test]
fn signal_030_track_and_fix_through_the_plugin_host() {
    let r = run(FAKE, &[], "t");
    let s = r.summary();
    assert_eq!(s.outcome, RunOutcome::Completed, "{:?}", r.log);
    assert_eq!(s.exit_code, Some(0));
    assert_eq!(
        (s.dwell_start_sample, s.dwell_end_sample),
        (0, (DWELL_S * RATE) as u64)
    );
    assert_eq!(
        s.epochs, 20,
        "2 s at the manifest's 100 ms observation rate"
    );
    assert_eq!(s.max_svs, 6);
    assert_eq!(s.fixes, 2, "fixes on whole seconds");
    assert_eq!(s.rejected, 0);

    let epochs = r.epochs();
    assert_eq!(epochs.len(), 20);
    let first = &epochs[0];
    // GPS 2026-09-22 10:00:00 is UTC 09:59:42 (18 leap seconds).
    let utc = (20_718i64 * 86_400 + 9 * 3600 + 59 * 60 + 42) * 1_000_000_000;
    assert_eq!(first.t.as_unix_nanos(), utc);
    assert!(first.locked_count() >= 4, "[SIGNAL-030] a fix needs four");
    for sv in first.locked() {
        assert!(sv.pseudorange_m.is_some_and(|p| p > 1.9e7), "{sv:?}");
        assert!(sv.carrier_phase_cycles.is_some(), "{sv:?}");
        assert!(sv.elevation_deg.is_some(), "{sv:?}");
    }
    let truth = geodetic_to_ecef(51.0 + 28.674 / 60.0, -(0.09 / 60.0), 45.0);
    let pos = first
        .position
        .expect("[SIGNAL-030] a fix at the first whole second");
    assert!(pos.distance_m(&truth) < 0.5, "{pos:?} vs {truth:?}");
    assert!(
        epochs[1].position.is_none(),
        "no fix is invented between PVT outputs"
    );
    r.assert_evidence_only();
}

/// PROP-033: S4 per satellite from the tracked C/N0 series — the scintillating satellite stands
/// out, steady ones read ~0.
#[test]
fn prop_033_s4_from_tracked_cn0() {
    let r = run(FAKE, &[], "s");
    let s4 = s4_by_prn(&r.epochs(), 10);
    assert_eq!(s4.len(), 6, "{s4:?}");
    assert!(s4[&24] > 0.5, "[PROP-033] PRN 24 scintillates: {s4:?}");
    for (prn, v) in &s4 {
        if *prn != 24 {
            assert!(*v < 0.01, "[PROP-033] PRN {prn} is steady: {s4:?}");
        }
    }
    r.assert_evidence_only();
}

/// AWARE-002: real per-satellite C/N0 from a tracking receiver corroborates the blind floor
/// rise — and, with the floor at baseline, the same loss reads as blockage, not jamming.
#[test]
fn aware_002_real_cn0_corroborates_jamming_and_separates_blockage() {
    let r = run(FAKE, &["--gnss-sdr-arg", "--fake-mode=jam"], "j");
    let epochs = r.epochs();
    let lock = lock_evidence(epochs.first().unwrap(), epochs.last().unwrap());
    assert_eq!((lock.svs_before, lock.svs_now), (6, 1));
    assert!(
        (lock.mean_cn0_drop_db - 12.0).abs() < 1e-3,
        "[AWARE-002] the measured C/N0 drop: {lock:?}"
    );

    let cfg = IntegrityConfig::default();
    let power = |rise: f32| PowerEvidence {
        observed_floor_dbfs: -60.0 + rise,
        baseline_floor_dbfs: -60.0,
        center_hz: L1_HZ,
        bandwidth_hz: RATE,
    };
    let jammed = assess_jamming(&power(10.0), Some(&lock), &cfg);
    assert_eq!(jammed.verdict, JammingVerdict::JammingSuspect);
    assert!(jammed.used_observables);
    let blind = assess_jamming(&power(10.0), None, &cfg);
    assert!(
        jammed.confidence > blind.confidence,
        "[AWARE-002] receiver C/N0 sharpens the blind verdict"
    );
    let blocked = assess_jamming(&power(0.0), Some(&lock), &cfg);
    assert_eq!(blocked.verdict, JammingVerdict::BlockageSuspect);
    r.assert_evidence_only();
}

/// A receiver that crashes still yields a dwell summary saying so, and the wrapper survives.
#[test]
fn a_failed_receiver_run_is_reported_not_hidden() {
    let r = run(FAKE, &["--gnss-sdr-arg", "--fake-mode=crash"], "c");
    let s = r.summary();
    assert_eq!(s.outcome, RunOutcome::Failed);
    assert_eq!(s.exit_code, Some(3));
    assert_eq!(s.epochs, 0);
    assert_eq!(
        r.crashes, 0,
        "the wrapper itself must not crash: {:?}",
        r.log
    );
}

/// A receiver that never finishes is killed after its budget and reported as timed out — and
/// the wrapper that killed it survives. This is also the regression for the host's macOS
/// `waitid` fix (T-323, `hk-plugins` `wait_exit_unreaped`): before it, the wrapper killing its
/// own child made `waitid` report the live wrapper as changed, and the host SIGKILLed it (5 of 6
/// runs in isolation).
#[test]
fn a_hung_receiver_is_killed_after_its_budget() {
    let r = run(
        FAKE,
        &["--gnss-sdr-arg", "--fake-mode=hang", "--timeout-s", "1"],
        "h",
    );
    let s = r.summary();
    assert_eq!(s.outcome, RunOutcome::TimedOut, "{:?}", r.log);
    assert_eq!(s.epochs, 0);
}

/// The real GNSS-SDR, when installed: the generated config is accepted and the run completes on
/// a noise-only dwell (no satellites, so no epochs — a fix needs a real L1 capture).
#[test]
fn real_gnss_sdr_accepts_the_generated_config() {
    let ok = Command::new("gnss-sdr")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("SKIP: gnss-sdr not found on PATH (the dev Mac has no formula for it)");
        return;
    }
    let r = run("gnss-sdr", &[], "r");
    let s = r.summary();
    assert_eq!(s.outcome, RunOutcome::Completed, "{:?}", r.log);
    r.assert_evidence_only();
}
