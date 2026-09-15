//! T-037b data-path follow-ups through the composed pipeline:
//! - the detection writer thread: a database locked by another connection does not stall
//!   detection, and every detection is still written afterwards;
//! - short lossless replays: a coverage chain attaches on a replay shorter than the ring and
//!   feeds every sample;
//! - plugin backpressure: in lossless replay a slow plugin is waited for, never dropped;
//! - replay dedup by capture name: the same IQ replayed again adds no track sightings, another
//!   capture with the same timestamps does;
//! - the FSK bits stream, gated like the other content streams.
//!
//! Every replay starts through `blind_replay_config` (SigMF annotations stripped; truth stays in
//! the test). The plugin tests use `hk-dummy-plugin` (built with the workspace) and skip without it.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use hk_core::Pacing;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::{DetectionId, IdentityScheme, InventoryQuery, Timestamp};
use hk_pipeline::{Candidate, ChainSpec, PipelineConfig, replay_plan};
use hk_stream::{Declared, Record, RecordFlags, StreamHeader, StreamKind, StreamReader};
use serde_json::json;

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `hk-dummy-plugin` next to the test binary.
fn dummy_plugin(dir: &Path) -> Option<PathBuf> {
    let cfg = PipelineConfig::new(dir, replay_plan(1090e6, 2.4e6, Timestamp::UNIX_EPOCH)).unwrap();
    let found = cfg
        .plugin_dirs
        .iter()
        .map(|d| d.join("hk-dummy-plugin"))
        .find(|p| p.is_file());
    if found.is_none() {
        eprintln!("SKIP: hk-dummy-plugin is not built next to the test binary");
    }
    found
}

/// A ci8 2.4 Msps dummy-plugin manifest with `args` and an input queue of `queue_bytes`.
fn dummy_manifest(dir: &Path, exe: &Path, args: &[&str], queue_bytes: usize) -> PathBuf {
    let m = json!({
        "manifest_version": 1,
        "id": "dummy",
        "version": "0.1.0",
        "licence": "LicenseRef-hackriff-undecided",
        "description": "T-037b data-path test plugin",
        "executable": exe.to_string_lossy(),
        "args": args,
        "input": {
            "kind": "iq",
            "datatype": "ci8",
            "sample_rates_hz": [2400000],
            "center_hz": { "min": 1000000, "max": 6000000000u64 }
        },
        "output": { "format": "ndjson", "schema_id": "hackriff.dummy/1", "content_class": "unrestricted" },
        "restart": { "backoff_initial_ms": 200, "backoff_max_ms": 30000, "max_restarts": 5, "window_s": 300 },
        "limits": {
            "input_queue_bytes": queue_bytes,
            "stall_timeout_ms": 60000,
            "max_message_bytes": 1048576,
            "stderr_lines": 200,
            "nice": 10
        }
    });
    let path = dir.join("dummy-manifest.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&m).unwrap()).unwrap();
    path
}

/// [`dummy_manifest`] plus `input.ready_signal` (T-223): the plugin promises a `ready` line, so a
/// lossless chain must hold its first record until it arrives.
fn dummy_ready_manifest(dir: &Path, exe: &Path, args: &[&str]) -> PathBuf {
    let path = dummy_manifest(dir, exe, args, 8 << 20);
    let mut m: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    m["input"]["ready_signal"] = json!(true);
    let path = dir.join("dummy-ready-manifest.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&m).unwrap()).unwrap();
    path
}

/// A plan extra whose only chain is a dummy-plugin coverage chain over `center ± 0.5 MHz`.
fn coverage_plan(center: f64, manifest: &Path) -> serde_json::Value {
    json!({ "pipeline": { "chains": [{
        "id": "dummy-coverage",
        "trigger": "coverage",
        "freq_hz": [[center - 0.5e6, center + 0.5e6]],
        "nodes": [{ "node": "plugin", "manifest": manifest.to_string_lossy(), "settle_s": 2.0 }]
    }] } })
}

#[test]
fn a_locked_database_does_not_stall_detection() {
    let src = TempDir::new("stall-src");
    let meta = tone_recording(&src.0, "tone", 250e3, 6.0, 433.5e6, None);
    let dir = TempDir::new("stall");
    let (cfg, replay, _input) =
        blind_replay_config(&dir.0, &meta, json!({}), Pacing::RealTime { speed: 1.0 });
    let handle = start(cfg, replay);
    let counters = handle.counters();
    let deadline = Instant::now() + Duration::from_secs(60);
    while counters.detect.detections_written.load(Ordering::Relaxed) == 0 {
        assert!(Instant::now() < deadline, "no detection was ever written");
        std::thread::sleep(Duration::from_millis(5));
    }
    // Another connection takes the write lock for 3 s (the repository's busy timeout is 5 s, so
    // the writer waits rather than failing).
    let conn = rusqlite::Connection::open(dir.0.join("hackriff.db")).unwrap();
    conn.busy_timeout(Duration::from_secs(10)).unwrap();
    conn.execute_batch("BEGIN EXCLUSIVE;").unwrap();
    let (frames0, lost0) = (
        counters.detect_reader.frames.load(Ordering::Relaxed),
        counters.detect_reader.lost_samples.load(Ordering::Relaxed),
    );
    let written0 = counters.detect.detections_written.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs(3));
    let frames1 = counters.detect_reader.frames.load(Ordering::Relaxed);
    let written1 = counters.detect.detections_written.load(Ordering::Relaxed);
    conn.execute_batch("COMMIT;").unwrap();

    let (s, fired) = wait_guarded(handle, Duration::from_secs(120));
    eprintln!("{}", s.to_text());
    assert!(!fired, "the run did not finish; the watchdog stopped it");
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    let r = s.resolution;
    let expected = 3.0 / r.frame_s;
    let during = (frames1 - frames0) as f64;
    eprintln!("{during} detection frames while the database was locked (≈{expected:.0} expected)");
    assert_eq!(
        written1, written0,
        "the lock held: nothing was written meanwhile"
    );
    assert!(
        during >= 0.6 * expected,
        "detection stalled behind the locked database: {during} frames in 3 s"
    );
    assert_eq!(lost0, 0);
    assert_eq!(s.counter("/readers/detect/lost_samples"), 0);
    assert_eq!(s.always_on_lost_samples, 0);
    assert!(s.counter("/detect/detections") > 0);
    assert_eq!(
        s.counter("/detect/detections_written"),
        s.counter("/detect/detections"),
        "every detection is written once the lock is released"
    );
    assert!(s.counter("/detect/track_rows") > 0);
}

#[test]
fn a_short_lossless_replay_attaches_its_coverage_chain_and_feeds_every_sample() {
    let src = TempDir::new("short-src");
    let dir = TempDir::new("short");
    let Some(exe) = dummy_plugin(&dir.0) else {
        return;
    };
    // 0.1 s: far shorter than the 4 s ring, so the lossless gate never holds capture on its own.
    let meta = tone_recording(&src.0, "short", 2.4e6, 0.1, 1090e6, None);
    let manifest = dummy_manifest(&src.0, &exe, &["--every", "1"], 8 << 20);
    let (cfg, replay, _input) = blind_replay_config(
        &dir.0,
        &meta,
        coverage_plan(1090e6, &manifest),
        Pacing::Unpaced,
    );
    assert!(cfg.lossless);
    let (s, fired) = wait_guarded(start(cfg, replay), Duration::from_secs(120));
    eprintln!("{}", s.to_text());
    assert!(!fired, "the run did not finish; the watchdog stopped it");
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    assert_eq!(s.counter("/source/samples"), 240_000);
    assert_eq!(
        s.counter("/chains/attached"),
        1,
        "the coverage chain attaches on a replay shorter than the ring"
    );
    assert_eq!(
        s.counter("/chains/plugin_samples"),
        s.counter("/source/samples"),
        "every sample reached the plugin"
    );
    assert_eq!(s.counter("/chains/plugin_dropped"), 0);
    assert!(
        s.counter("/chains/plugin_decodes") > 0,
        "the plugin decoded"
    );
    assert_eq!(s.counter("/source/coverage_wait_timeouts"), 0);
}

/// T-223: a plugin whose manifest declares `input.ready_signal` is not fed until it has said it
/// is ready. `Running` (its process is attached) is not enough: the readsb wrapper is attached
/// long before readsb's Beast connection exists, and under load the squitters fed in between were
/// decoded without their sample time. Here the plugin reports ready 2 s after it starts; before
/// the fix the chain fed it from the first record, so `plugin_fed_before_ready` was non-zero.
#[test]
fn a_plugin_that_declares_readiness_is_not_fed_until_it_is_ready() {
    let src = TempDir::new("ready-src");
    let dir = TempDir::new("ready");
    let Some(exe) = dummy_plugin(&dir.0) else {
        return;
    };
    let meta = tone_recording(&src.0, "short", 2.4e6, 0.1, 1090e6, None);
    let manifest = dummy_ready_manifest(
        &src.0,
        &exe,
        &[
            "--every",
            "1",
            "--profile",
            "adsb-like",
            "--ready-after-ms",
            "2000",
        ],
    );
    let (cfg, replay, _input) = blind_replay_config(
        &dir.0,
        &meta,
        coverage_plan(1090e6, &manifest),
        Pacing::Unpaced,
    );
    let (s, fired) = wait_guarded(start(cfg, replay), Duration::from_secs(120));
    eprintln!("{}", s.to_text());
    assert!(!fired, "the run did not finish; the watchdog stopped it");
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    assert_eq!(s.counter("/chains/attached"), 1);
    assert_eq!(
        s.counter("/chains/plugin_fed_before_ready"),
        0,
        "records reached the plugin before it reported ready"
    );
    assert_eq!(
        s.counter("/chains/plugin_ready_timeouts"),
        0,
        "the lossless chain gave up waiting for readiness"
    );
    // Holding the first record loses nothing: the gate cursor holds capture while the chain waits.
    assert_eq!(s.counter("/chains/plugin_dropped"), 0);
    assert_eq!(
        s.counter("/chains/plugin_samples"),
        s.counter("/source/samples")
    );
    assert!(
        s.counter("/chains/plugin_decodes") > 0,
        "the plugin decoded nothing"
    );
}

/// T-103: a plugin that takes longer to start reading than the old 2 s settle window (the flaky
/// readsb chain under load, B0.206/B0.219) still decodes every record, the first included. Before
/// the fix the chain stopped it after 2 s without a decode: 0 decodes, every time.
#[test]
fn a_slow_starting_plugin_decodes_every_record_from_the_first() {
    let src = TempDir::new("slowstart-src");
    let dir = TempDir::new("slowstart");
    let Some(exe) = dummy_plugin(&dir.0) else {
        return;
    };
    let meta = tone_recording(&src.0, "short", 2.4e6, 0.1, 1090e6, None);
    let manifest = dummy_manifest(
        &src.0,
        &exe,
        &[
            "--every",
            "1",
            "--profile",
            "adsb-like",
            "--start-delay-ms",
            "3000",
        ],
        8 << 20,
    );
    let (cfg, replay, _input) = blind_replay_config(
        &dir.0,
        &meta,
        coverage_plan(1090e6, &manifest),
        Pacing::Unpaced,
    );
    let (s, fired) = wait_guarded(start(cfg, replay), Duration::from_secs(120));
    eprintln!("{}", s.to_text());
    assert!(!fired, "the run did not finish; the watchdog stopped it");
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    assert_eq!(s.counter("/chains/attached"), 1);
    assert_eq!(s.counter("/chains/plugin_dropped"), 0);
    assert_eq!(
        s.counter("/chains/plugin_samples"),
        s.counter("/source/samples")
    );
    // One decode per record (`--every 1`), each carrying how many records the plugin had read.
    let repo = repo(&dir.0);
    let mut records: Vec<u64> = ["a1b2c0", "a1b2c1", "a1b2c2", "a1b2c3"]
        .iter()
        .flat_map(|icao| {
            repo.decodes_for_identity(&hk_model::DecodedIdentity {
                scheme: IdentityScheme::AdsbIcao,
                value: (*icao).to_owned(),
            })
            .unwrap()
        })
        .map(|d| d.metadata["records"].as_u64().unwrap())
        .collect();
    records.sort_unstable();
    assert!(
        s.counter("/chains/plugin_decodes") > 0,
        "the slow-starting plugin decoded nothing"
    );
    assert_eq!(records.first(), Some(&1), "the first record was decoded");
    assert_eq!(
        records,
        (1..=records.len() as u64).collect::<Vec<_>>(),
        "every record the plugin read was decoded, none lost"
    );
    assert_eq!(s.counter("/chains/plugin_decodes"), records.len() as u64);
}

#[test]
fn lossless_plugin_feeding_waits_for_a_slow_plugin_instead_of_dropping() {
    let src = TempDir::new("slow-src");
    let dir = TempDir::new("slow");
    let Some(exe) = dummy_plugin(&dir.0) else {
        return;
    };
    // 3 s at 2.4 Msps is 14.4 MiB of ci8, fourteen times the plugin's 1 MiB input queue, fed
    // to a plugin that sleeps 3 ms per record.
    let meta = tone_recording(&src.0, "long", 2.4e6, 3.0, 1090e6, None);
    let manifest = dummy_manifest(
        &src.0,
        &exe,
        &["--every", "64", "--read-delay-us", "3000"],
        1 << 20,
    );
    let (cfg, replay, _input) = blind_replay_config(
        &dir.0,
        &meta,
        coverage_plan(1090e6, &manifest),
        Pacing::Unpaced,
    );
    let (s, fired) = wait_guarded(start(cfg, replay), Duration::from_secs(300));
    eprintln!("{}", s.to_text());
    assert!(!fired, "the run did not finish; the watchdog stopped it");
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    assert_eq!(s.counter("/chains/attached"), 1);
    assert!(
        s.counter("/chains/plugin_waits") > 0,
        "the plugin's queue filled and the chain waited"
    );
    assert_eq!(s.counter("/chains/plugin_wait_timeouts"), 0);
    assert_eq!(
        s.counter("/chains/plugin_dropped"),
        0,
        "lossless replay drops no plugin input"
    );
    assert_eq!(
        s.counter("/chains/plugin_samples"),
        s.counter("/source/samples")
    );
    assert_eq!(s.always_on_lost_samples, 0);
}

#[test]
fn replaying_the_same_iq_again_does_not_count_its_tracks_twice() {
    let src = TempDir::new("dedup-src");
    let meta = tone_recording(&src.0, "tone", 250e3, 2.0, 433.5e6, None);
    // Identical samples and timestamps, another stream index: a different capture.
    let other = tone_recording(&src.0, "tone-other", 250e3, 2.0, 433.5e6, Some(1_000_000));
    let dir = TempDir::new("dedup");
    let run_blind = |meta: &Path| {
        let (cfg, replay, _input) = blind_replay_config(&dir.0, meta, json!({}), Pacing::Unpaced);
        let s = start(cfg, replay).wait().unwrap();
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        s
    };
    let counts = || {
        let repo = repo(&dir.0);
        let entries = inventory(&repo, InventoryQuery::default());
        let total: u64 = entries.iter().map(|e| e.emitter.count).sum();
        (entries.len(), total)
    };
    let s1 = run_blind(&meta);
    assert!(s1.counter("/detect/tracks_closed") > 0);
    let first = counts();
    assert!(first.1 > 0, "the tone's track reached the inventory");
    run_blind(&meta);
    assert_eq!(
        counts(),
        first,
        "the same capture replayed again adds no sightings"
    );
    run_blind(&other);
    let third = counts();
    assert!(
        third.1 > first.1,
        "another capture with the same timestamps still counts: {first:?} → {third:?}"
    );
}

/// Plugin decodes reach the T-039 family step: the chain classifies the emitters its identity
/// decodes resolved to by the plugin's id (`readsb` → ADS-B). `hk-dummy-plugin --profile
/// adsb-like` stands in for readsb under a manifest named `readsb` (CI has no readsb; the real
/// chain is checked in `signal_001_readsb.rs`). Legal: under a metadata-only source the same
/// plugin output adds a family explanation and nothing else: identities stay withheld and the
/// explanations name no aircraft.
#[test]
fn plugin_decodes_get_family_explanations_that_reveal_nothing_more_when_gated() {
    let src = TempDir::new("family-src");
    let dir = TempDir::new("family");
    let Some(exe) = dummy_plugin(&dir.0) else {
        return;
    };
    let manifest = dummy_manifest(
        &src.0,
        &exe,
        &["--every", "1", "--profile", "adsb-like"],
        8 << 20,
    );
    let mut m: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
    m["id"] = json!("readsb");
    m["output"]["identity"] = json!({ "scheme": "adsb-icao", "charset": "hex", "max_len": 6 });
    m["output"]["frame_models"] = json!(["adsb-df17"]);
    std::fs::write(&manifest, serde_json::to_vec_pretty(&m).unwrap()).unwrap();
    let icaos = ["a1b2c0", "a1b2c1", "a1b2c2", "a1b2c3"];
    let run_at = |center: f64, dir: &Path| {
        let meta = tone_recording(&src.0, &format!("t{center}"), 2.4e6, 0.5, center, None);
        let (cfg, replay, _input) = blind_replay_config(
            dir,
            &meta,
            coverage_plan(center, &manifest),
            Pacing::Unpaced,
        );
        let class = cfg.source_class;
        let (s, fired) = wait_guarded(start(cfg, replay), Duration::from_secs(120));
        eprintln!("{}", s.to_text());
        assert!(!fired && s.errors.is_empty(), "{:?}", s.errors);
        assert!(s.counter("/chains/plugin_decodes") > 0);
        class
    };

    // 1090 MHz (unrestricted, ADS-B allocation): every aircraft's top explanation is ADS-B.
    assert!(run_at(1090e6, &dir.0).permits_content());
    let repo1 = repo(&dir.0);
    let aircraft = inventory(
        &repo1,
        InventoryQuery {
            identity_scheme: Some(IdentityScheme::AdsbIcao),
            ..InventoryQuery::default()
        },
    );
    assert_eq!(aircraft.len(), icaos.len(), "one emitter per aircraft");
    for e in &aircraft {
        let ranked = hk_pipeline::explanations(&repo1, e.emitter.id).unwrap();
        assert_eq!(
            ranked.first().map(|x| x.service.as_str()),
            Some("adsb"),
            "{ranked:?}"
        );
    }
}

/// T-055 HIL: `analog chain write: FOREIGN KEY constraint failed` when the detect reader overran
/// at 8–10 Msps, because chain rows named a triggering detection that was not (yet) stored.
/// Deterministic here: a runtime analog chain names a detection that is never stored. After the
/// bounded wait the reference is dropped and counted; the Demodulation is still written, no write
/// is refused by the foreign key, and no row names a missing detection.
#[test]
fn chain_rows_whose_trigger_detection_never_arrives_are_written_without_it_and_counted() {
    let src = TempDir::new("parent-src");
    let meta = tone_recording(&src.0, "fm-tone", 250e3, 4.0, 100.1e6, None);
    let dir = TempDir::new("parent");
    let (cfg, replay, _input) = blind_replay_config(
        &dir.0,
        &meta,
        json!({ "pipeline": { "chains": [] } }),
        Pacing::RealTime { speed: 1.0 },
    );
    assert!(cfg.source_class.permits_content(), "FM band prior");
    let center = replay.info.center_hz;
    let handle = start(cfg, replay);
    std::thread::sleep(Duration::from_millis(300));
    let at = handle.ring_position().saturating_sub(4096);
    let spec: ChainSpec = serde_json::from_value(json!({
        "id": "analog-missing-parent",
        "requires_content": true,
        "nodes": [{ "node": "analog-auto", "pre_s": 0.0, "window_s": 1.0,
                    "bandwidth_hz": 40e3, "probe_s": 0.0 }]
    }))
    .unwrap();
    let never_stored = DetectionId::new();
    handle.attach_chain(
        spec,
        Candidate {
            track: None,
            detection: Some(never_stored),
            f_lo_hz: center + 30e3,
            f_hi_hz: center + 70e3,
            first_sample: at,
            trigger_sample: at,
            bursty: Some(false),
        },
    );
    let (s, fired) = wait_guarded(handle, Duration::from_secs(120));
    eprintln!("{}", s.to_text());
    assert!(!fired, "the run did not finish; the watchdog stopped it");
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    assert_eq!(s.counter("/chains/attached"), 1);
    assert_eq!(s.counter("/chains/detection_ref_missing"), 1);
    assert_eq!(
        s.counter("/chains/errors"),
        0,
        "no chain write refused by the foreign key"
    );
    assert_eq!(
        s.counter("/chains/demodulations"),
        1,
        "the rows are written without the missing parent"
    );
    let conn = rusqlite::Connection::open(dir.0.join("hackriff.db")).unwrap();
    let orphans: i64 = conn
        .query_row(
            "SELECT count(*) FROM demodulation m LEFT JOIN detection d \
             ON d.detection_id = m.detection_id \
             WHERE m.detection_id IS NOT NULL AND d.detection_id IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphans, 0, "no demodulation names a missing detection");
}

/// `(flags, payload)` of every binary record in a captured stream, with its header.
fn binary_records(bytes: &[u8]) -> (StreamHeader, Vec<(u8, Vec<u8>)>) {
    let mut reader = StreamReader::new(bytes);
    let header = reader.read_header().unwrap().clone();
    let mut out = Vec::new();
    while let Some(record) = reader.next_record().unwrap() {
        if let Record::Binary(b) = record {
            out.push((b.header.flags.0, b.payload.to_vec()));
        }
    }
    (header, out)
}

fn hex_bits(hex: &str) -> Vec<u8> {
    (0..hex.len() / 2)
        .flat_map(|i| {
            let byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap();
            (0..8).rev().map(move |k| (byte >> k) & 1)
        })
        .collect()
}

#[test]
fn fsk_bits_are_published_like_the_other_content_streams() {
    let out = synth_or_skip!(
        SynthRequest::new("fsk_burst_train")
            .seed(36)
            .param("snr_db", 20.0)
            .param("duration_s", 2.4)
    );
    let fx = out.fixture(0).unwrap();
    let truths = fx.of_kind("fsk-burst");
    let payloads: Vec<String> = truths
        .iter()
        .map(|t| t.value["frame"]["payload_hex"].as_str().unwrap().to_owned())
        .collect();
    let n = truths.len();
    let run_bits = |extra: serde_json::Value| {
        let dir = TempDir::new("bits");
        let (mut cfg, replay, _input) =
            blind_replay_config(&dir.0, &fx.meta_path, extra, Pacing::Unpaced);
        let taps: Arc<Mutex<Vec<(StreamHeader, Buf)>>> = Arc::default();
        let sink_taps = Arc::clone(&taps);
        cfg.stream_sink = Some(Arc::new(move |h, handle| {
            if h.kind == StreamKind::Bits {
                let buf = Buf::default();
                handle
                    .subscribe("test-bits", Declared::local(buf.clone()), Box::new(|_| {}))
                    .unwrap();
                sink_taps.lock().unwrap().push((h.clone(), buf));
            }
        }));
        let s = start(cfg, replay).wait().unwrap();
        eprintln!("{}", s.to_text());
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        let captured: Vec<Vec<u8>> = taps
            .lock()
            .unwrap()
            .iter()
            .map(|(_, b)| b.0.lock().unwrap().clone())
            .collect();
        (s, captured)
    };

    // Classified by the user (own test sensor): the bits are delivered.
    let (s2, streams2) = run_bits(json!({ "pipeline": { "classify": [{
        "freq_hz": [433.8e6, 434.1e6],
        "content_class": "unrestricted",
        "by": "test: own synthetic AWARE-036 sensor"
    }] } }));
    let delivered: Vec<Vec<u8>> = streams2
        .iter()
        .flat_map(|b| binary_records(b).1)
        .filter(|(flags, _)| flags & RecordFlags::GATED.0 == 0)
        .map(|(_, payload)| payload)
        .collect();
    assert_eq!(delivered.len() as u64, s2.counter("/chains/bits_records"));
    assert!(
        delivered.len() * 10 >= n * 8,
        "bits for {} of {n} bursts",
        delivered.len()
    );
    assert!(
        delivered
            .iter()
            .all(|p| !p.is_empty() && p.iter().all(|&b| b <= 1))
    );
    // Matched against the truth the test holds: each payload's bits (either polarity) appear in
    // a delivered burst.
    let found = payloads
        .iter()
        .filter(|hex| {
            let bits = hex_bits(hex);
            let inverted: Vec<u8> = bits.iter().map(|b| 1 - b).collect();
            delivered.iter().any(|p| {
                p.windows(bits.len())
                    .any(|w| w == bits.as_slice() || w == inverted.as_slice())
            })
        })
        .count();
    assert!(
        found * 10 >= n * 8,
        "truth payload bits found in {found} of {n} bursts"
    );
}
