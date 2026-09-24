//! T-275 (SIGNAL-025, SIGNAL-027, SIGNAL-028, SIGNAL-029): the SatDump plugin wrapper end to end,
//! driving `hk-plugin-satdump` and the T-014 plugin host against `hk-fake-satdump` (module doc,
//! `src/bin/hk-fake-satdump.rs`) rather than a real SatDump install — there is no fixture that
//! puts a real polar-orbiter or GOES pass in front of a dev machine's antenna, and SatDump's own
//! DSP chain is not this repo's to test. What these tests exercise is the wrapper's own contract:
//! FIFO hand-off, product-file reporting as a `decode` row, and crash isolation, exactly as
//! `hk-plugin-readsb`'s tests exercise readsb's wrapper contract using `hk-fake-readsb` for the
//! same reason.
//!
//! See `docs/adr/0010-language-and-licence-ledger.md` for SatDump's licence (GPL-3.0-or-later) and
//! why it only ever runs as a subprocess.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hk_model::sigmf::Datatype;
use hk_model::{ContentClass, Repository, SampleTime, Timestamp};
use hk_plugins::{
    HostError, Ingest, InputStreamDesc, ManifestError, PluginContext, PluginInstance,
    PluginManifest, PluginMonitor, PluginState, RestartPolicy,
};
use hk_stream::{
    BinaryRecord, Listener, Publisher, PublisherConfig, Record, RecordFlags, StreamHeader,
    StreamKind, StreamReader,
};

/// Serialises every test in this file that spawns a `PluginInstance` against the ones that
/// temporarily set `HK_SATDUMP`/`FAKE_SATDUMP_*` process environment (the same reasoning as
/// `readsb.rs`'s `ENV_MUTEX`: `std::env::set_var` racing a spawn that expects the unmodified
/// environment is unsound, not just flaky).
static ENV_MUTEX: Mutex<()> = Mutex::new(());

mod common;

const WRAPPER: &str = env!("CARGO_BIN_EXE_hk-plugin-satdump");
const FAKE_SATDUMP: &str = env!("CARGO_BIN_EXE_hk-fake-satdump");
const CENTER_HZ: f64 = 137_100_000.0;
const RATE: f64 = 1_024_000.0;
const CHUNK_SAMPLES: usize = 4096;

/// The repository's 137 MHz SatDump manifest, pointed at the freshly built wrapper binary.
fn manifest() -> PluginManifest {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/satdump-137/manifest.json");
    let mut m = PluginManifest::load(path).unwrap();
    m.executable = WRAPPER.into();
    m
}

fn input(anchor: SampleTime) -> InputStreamDesc {
    InputStreamDesc {
        datatype: Datatype::Ci8,
        sample_rate_hz: RATE,
        center_hz: Some(CENTER_HZ),
        bandwidth_hz: Some(RATE),
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

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hksd{tag}{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait_running(inst: &PluginInstance) {
    let mon = inst.monitor();
    common::wait_started(&mon);
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

/// Feeds pseudo-random `ci8` bytes (non-zero: the fake plugin's `product_after_bytes` mode counts
/// real bytes read off the fifo) in fixed-size chunks starting at `start_sample`.
fn push_bytes(inst: &mut PluginInstance, total_bytes: usize, start_sample: u64) -> u64 {
    let bps = Datatype::Ci8.bytes_per_sample();
    let mut sample = start_sample;
    let mut remaining = total_bytes;
    let mut seed = 0x1234_5678u32;
    while remaining > 0 {
        let n = remaining.min(CHUNK_SAMPLES * bps);
        let chunk: Vec<u8> = (0..n)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((seed >> 24) as u8).max(1) // never all-zero: satdump reads real IQ, not silence
            })
            .collect();
        inst.push(BinaryRecord {
            t: Timestamp::now(),
            sample_index: sample,
            flags: RecordFlags::empty(),
            payload: &chunk,
        })
        .unwrap();
        sample += n as u64 / bps as u64;
        remaining -= n;
    }
    sample
}

/// T-275: replaying enough IQ through the SatDump wrapper lands one product (SatDump's `png`
/// output, once `hk-fake-satdump`'s `product_after_bytes` threshold is crossed and its file stops
/// growing) as a `decode` row, and it republishes over a T-016 messages stream carrying the
/// product's file path in `content.path` — this is what SIGNAL-025/027/028/029's shared decoder
/// reuse (docs/use-cases.yaml, T-275) actually delivers to the pipeline.
#[test]
fn satdump_product_lands_as_a_decode_and_republishes() {
    let _guard = ENV_MUTEX.lock().unwrap();
    // SAFETY: serialised by ENV_MUTEX against every other test in this file that spawns a
    // `PluginInstance` or touches these variables.
    unsafe {
        std::env::set_var("HK_SATDUMP", FAKE_SATDUMP);
        std::env::set_var("FAKE_SATDUMP_MODE", "product_after_bytes");
        std::env::set_var("FAKE_SATDUMP_PRODUCT_AFTER_BYTES", "4096");
    }

    let dir = temp_dir("s1");
    let path = dir.join("satdump.sock");
    let mut header = StreamHeader::new(
        "decodes/satdump-137",
        StreamKind::Messages,
        ContentClass::Unrestricted,
        "hk-plugins:satdump@0.1.0",
    );
    header.message_schema = Some("hackriff.satdump-137/1".into());
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
    push_bytes(&mut inst, 64 * 1024, 0);
    let mon = inst.monitor();
    wait(&mon, "one satdump product decode", |s| s.decodes >= 1);
    let final_stats = mon.stats();
    drop(mon);
    inst.shutdown();

    let mut ingest = Arc::try_unwrap(sink)
        .ok()
        .expect("host released the ingest")
        .into_inner()
        .unwrap();
    drop(ingest.take_publisher());
    let (_stream_header, records) = consumer.join().unwrap();
    drop(listener);

    assert!(final_stats.decodes >= 1, "{final_stats:?}");
    assert_eq!(final_stats.malformed, 0, "{final_stats:?}");

    let product = records
        .iter()
        .find_map(|r| match r {
            Record::Message(env) if env.value["frame_model"] == "satdump-product" => {
                Some(env.value.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no satdump-product decode republished: {records:?}"));
    let path = product["content"]["path"].as_str().expect("content.path");
    assert!(
        path.ends_with("product.png"),
        "unexpected product path: {path}"
    );
    assert_eq!(product["metadata"]["kind"], "png");
}

/// The repository's L-band SatDump manifest (SIGNAL-025/028, needs-accessory tier), pointed at
/// the freshly built wrapper binary.
fn lband_manifest() -> PluginManifest {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/satdump-lband/manifest.json");
    let mut m = PluginManifest::load(path).unwrap();
    m.executable = WRAPPER.into();
    m
}

/// T-275: both manifests parse (JSON, required fields, licence, content class) and accept the
/// input each tier is meant for; the 137 MHz manifest refuses an L-band centre frequency and
/// vice versa, so the tiering docs/use-cases.yaml states (native vs. needs-accessory) is what a
/// caller actually gets refused or accepted on, not just prose. No subprocess is spawned:
/// `check_input` runs before the host ever execs the wrapper.
#[test]
fn both_manifests_load_and_enforce_their_own_frequency_tier() {
    let sink = || {
        Arc::new(Mutex::new(Ingest::new(
            Repository::open_in_memory().unwrap(),
        )))
    };

    // The 137 MHz manifest accepts SIGNAL-027/029's band...
    assert!(
        PluginInstance::spawn(
            manifest(),
            input(anchor()),
            PluginContext::default(),
            sink()
        )
        .is_ok()
    );
    // ...and refuses an L-band centre (SIGNAL-025/028's ~1.7 GHz).
    let mut lband_freq = input(anchor());
    lband_freq.center_hz = Some(1_694_100_000.0);
    let err = PluginInstance::spawn(manifest(), lband_freq, PluginContext::default(), sink())
        .err()
        .expect("137 MHz manifest refuses an L-band centre");
    assert!(matches!(
        err,
        HostError::Manifest(ManifestError::Mismatch(_))
    ));

    // The L-band manifest accepts SIGNAL-025/028's band...
    let mut lband_input = input(anchor());
    lband_input.center_hz = Some(1_694_100_000.0);
    assert!(
        PluginInstance::spawn(
            lband_manifest(),
            lband_input,
            PluginContext::default(),
            sink()
        )
        .is_ok()
    );
    // ...and refuses SIGNAL-027/029's 137 MHz centre.
    let err = PluginInstance::spawn(
        lband_manifest(),
        input(anchor()),
        PluginContext::default(),
        sink(),
    )
    .err()
    .expect("L-band manifest refuses a 137 MHz centre");
    assert!(matches!(
        err,
        HostError::Manifest(ManifestError::Mismatch(_))
    ));
}

/// T-275 fault injection: a SatDump that exits immediately (never opens the input FIFO) is
/// treated as a crash, isolated to this one plugin instance, and the host restarts the pair — the
/// same isolation `readsb_child_crash_mid_stream_is_isolated_and_recovers` checks for readsb.
#[test]
fn satdump_crash_is_isolated_and_the_host_restarts_it() {
    let _guard = ENV_MUTEX.lock().unwrap();
    // SAFETY: see the module doc's ENV_MUTEX note.
    unsafe {
        std::env::set_var("HK_SATDUMP", FAKE_SATDUMP);
        std::env::set_var("FAKE_SATDUMP_MODE", "crash_immediately");
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
    let inst = PluginInstance::spawn(m, input(anchor()), PluginContext::default(), sink).unwrap();
    let mon = inst.monitor();
    wait(&mon, "a crash to be recorded", |s| s.crashes >= 1);
    wait(&mon, "the host to restart the pair", |s| s.starts >= 2);
    let stats = mon.stats();
    drop(mon);
    inst.shutdown();
    assert!(stats.crashes >= 1, "{stats:?}");
    assert!(stats.starts >= 2, "{stats:?}");
}
