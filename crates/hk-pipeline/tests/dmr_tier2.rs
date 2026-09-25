//! T-989: a **conventional DMR repeater** found blind, through the mock SDR, during a normal run.
//!
//! # What this is testing, and what it is not
//!
//! `tests/e2e/.../t271_dmr.rs` already proves the trunking hunt can find a DMR **Tier III control
//! channel**. A conventional repeater has no control channel, so that hunt never reaches it: the
//! explorer window of 2026-09-25 watched a +24 dB DMR repeater at 464.6125 MHz — 42 base-station
//! data syncs in ten seconds by an independent oracle — and every burst arrived in the inventory
//! with `classification: null`. This scene is that emission, and the assertions are that a normal
//! run now says what it is.
//!
//! # Blind
//!
//! The recording carries **no annotations**: the mock device serves 4FSK bursts and nothing else.
//! The colour code, the talkgroup, the source address and the frequency are the test's private
//! truth, checked against what the run produced — never handed to it, and never looked up in a
//! band plan (the run is at 464.3 MHz with no allocation data of any kind).
//!
//! # The scene
//!
//! One repeater downlink: 30 ms TDMA slots, each a CACH and a 264-bit burst, carrying a voice LC
//! header, a packet-data header, voice superframes and a terminator, with idle bursts in the other
//! slot — built field by field by `hk_detect::dmr_tier2::encode` and modulated as real 4FSK
//! (±1944 / ±648 Hz at 4800 Bd) into ci8 IQ with noise.

mod common;

use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use hk_core::{MockEnd, Pacing};
use hk_detect::dmr_tier2::encode::repeater_downlink;
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_model::{AnnotationTarget, InventoryQuery};
use hk_pipeline::{Pipeline, PipelineConfig, TrackInventory, open_mock_replay, replay_plan};
use hk_stream::Declared;

const FS: f64 = 240_000.0;
const CENTER_HZ: f64 = 464.300e6;
/// The repeater's own frequency — the test's truth, never given to the run.
const EMISSION_HZ: f64 = 464.325e6;
/// Its colour code, talkgroup and source unit: the truth the headers have to state.
const COLOUR_CODE: u8 = 3;
const TALKGROUP: u32 = 0x00_2A_F8;
const SOURCE_UNIT: u32 = 0x12_34_56;
/// Slots on the air: 30 ms each, so 40 slots is 1.2 s of transmission.
const SLOTS: usize = 40;
/// Where the transmission starts in the 2 s recording, s.
const START_S: f64 = 0.30;
const SECS: f64 = 2.0;
const LIMIT: Duration = Duration::from_secs(180);

/// DMR's 4FSK deviations, Hz, in dibit order 0..=3 (`00` +648, `01` +1944, `10` −648, `11` −1944).
const DEVIATIONS_HZ: [f64; 4] = [648.0, 1944.0, -648.0, -1944.0];

/// The stream's records: a 4-byte little-endian length, then that many bytes of JSON.
fn frames(bytes: &[u8]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 4 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        at += 4;
        if at + len > bytes.len() {
            break;
        }
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes[at..at + len]) {
            out.push(v);
        }
        at += len;
    }
    out
}

/// A subscriber to the run's own message stream — what another program would attach to.
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Writes the scene as a ci8 SigMF recording with **no annotations** and returns its meta path.
fn scene(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let dibits = repeater_downlink(SLOTS, COLOUR_CODE, TALKGROUP, SOURCE_UNIT);
    let sps = FS / hk_detect::dmr_tier2::DMR_SYMBOL_RATE_BD;
    assert_eq!(
        sps, 50.0,
        "the scene wants a whole number of samples per symbol"
    );
    let n = (SECS * FS) as usize;
    let start = (START_S * FS) as usize;

    // Per-sample frequency, smoothed over a quarter symbol so the emission has a real spectrum
    // rather than the infinite one a hard frequency step would have.
    let mut freq = vec![0f64; n];
    for (k, d) in dibits.iter().enumerate() {
        let s0 = start + (k as f64 * sps) as usize;
        for i in 0..sps as usize {
            if s0 + i < n {
                freq[s0 + i] = DEVIATIONS_HZ[usize::from(*d)];
            }
        }
    }
    let span = sps as usize / 4;
    let smooth: Vec<f64> = (0..n)
        .map(|i| {
            let lo = i.saturating_sub(span / 2);
            let hi = (i + span / 2 + 1).min(n);
            freq[lo..hi].iter().sum::<f64>() / (hi - lo) as f64
        })
        .collect();

    let on = start..(start + (dibits.len() as f64 * sps) as usize).min(n);
    let offset = EMISSION_HZ - CENTER_HZ;
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut noise = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        ((state >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 10.0
    };
    let mut phase = 0f64;
    let mut data = Vec::with_capacity(2 * n);
    for (i, f) in smooth.iter().enumerate() {
        phase += std::f64::consts::TAU * (offset + f) / FS;
        let (re, im) = if on.contains(&i) {
            (50.0 * phase.cos(), 50.0 * phase.sin())
        } else {
            (0.0, 0.0)
        };
        data.push((re + noise()).round().clamp(-128.0, 127.0) as i8 as u8);
        data.push((im + noise()).round().clamp(-128.0, 127.0) as i8 as u8);
    }
    std::fs::write(dir.join("dmr.sigmf-data"), data).unwrap();
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(FS);
    meta.captures.push(Capture {
        sample_start: 0,
        frequency: Some(CENTER_HZ),
        datetime: Some("2026-09-25T06:30:00Z".into()),
        provenance: None,
        clip_count: None,
        extra: serde_json::Map::new(),
    });
    let path = dir.join("dmr.sigmf-meta");
    meta.write(&path).unwrap();
    assert!(meta.annotations.is_empty(), "the run is served no truth");
    path
}

#[test]
fn t989_a_normal_run_identifies_a_conventional_dmr_repeater_and_reads_its_headers() {
    let dir = TempDir::new("t989-dmr-tier2");
    let meta = scene(&dir.0.join("rec"));
    let replay = open_mock_replay(&meta, Pacing::Unpaced, MockEnd::Stop).unwrap();
    let info = replay.info;
    let mut cfg = PipelineConfig::new(
        &dir.0,
        replay_plan(info.center_hz, info.sample_rate_hz, info.start_time),
    )
    .unwrap();
    cfg.source_class = replay.class;
    cfg.lossless = true;
    // Subscribe to the DMR header stream the moment the chain offers it, as an external consumer
    // of the decoded headers would (docs/api.md's `messages` kind).
    let sink = Sink::default();
    let subscriber = sink.clone();
    cfg.stream_sink = Some(Arc::new(move |h, handle| {
        if h.stream_id.starts_with("messages/dmr-tier2/") {
            handle
                .subscribe(
                    "t989-consumer",
                    Declared::local(subscriber.clone()),
                    Box::new(|_| {}),
                )
                .unwrap();
        }
    }));
    let handle = Pipeline::start(
        cfg,
        Box::new(replay.source),
        info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let counters = handle.counters();
    let (summary, fired) = wait_guarded(handle, LIMIT);
    assert!(!fired, "the run stopped cleanly");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);

    let chains = &counters.chains;
    assert!(
        chains.dmr_scanned.load(Ordering::Relaxed) >= 1,
        "the region was never scanned for DMR"
    );
    assert!(
        chains.dmr_identified.load(Ordering::Relaxed) >= 1,
        "a DMR repeater on the air was not identified"
    );

    // The verdict, on the emitter's own row.
    let repo = repo(&dir.0);
    let entries = inventory(&repo, InventoryQuery::default());
    let rows: Vec<(f64, hk_model::Annotation)> = entries
        .iter()
        .flat_map(|e| {
            repo.annotations_for(&AnnotationTarget::Emitter(e.emitter.id))
                .unwrap()
                .into_iter()
                .map(move |a| (e.emitter.f_center_hz, a))
        })
        .filter(|(_, a)| a.author_ref == "hk-detect/dmr-tier2@0.1.0")
        .collect();
    assert_eq!(rows.len(), 1, "want exactly one DMR verdict, got {rows:?}");
    let (f_center, row) = &rows[0];
    assert!(
        (f_center - EMISSION_HZ).abs() < 12_500.0,
        "the verdict is on an emitter at {:.4} MHz, not the repeater's {:.4} MHz",
        f_center / 1e6,
        EMISSION_HZ / 1e6
    );
    assert!(
        row.value
            .starts_with(&format!("DMR Tier II, CC {COLOUR_CODE}, sync ")),
        "verdict: {}",
        row.value
    );
    assert_eq!(row.content, None, "a verdict carries no content");
    assert_eq!(row.content_class, hk_model::ContentClass::MetadataOnly);

    let scan = &row.metadata["dmr_tier2"];
    assert_eq!(scan["protocol"], "dmr-tier2");
    assert_eq!(scan["colour_code"], u64::from(COLOUR_CODE));
    assert!(
        scan["syncs"].as_u64().unwrap() >= 2,
        "syncs: {}",
        scan["syncs"]
    );
    assert_eq!(scan["bptc_failed"], 0);
    // Both base-station sync words are on the air, and the run says which it saw.
    let syncs: Vec<String> = scan["sync_words"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["sync"].as_str().unwrap().to_string())
        .collect();
    assert!(syncs.contains(&"bs-data".to_string()), "{syncs:?}");

    // The headers, against the truth only this test holds.
    let headers = scan["headers"].as_array().unwrap();
    let lc = headers
        .iter()
        .find(|h| h["header"] == "voice-lc-header")
        .unwrap_or_else(|| panic!("no voice LC header among {headers:?}"));
    assert_eq!(lc["destination"], u64::from(TALKGROUP), "talkgroup");
    assert_eq!(lc["source"], u64::from(SOURCE_UNIT), "source unit");
    let data = headers
        .iter()
        .find(|h| h["header"] == "data-header")
        .unwrap_or_else(|| panic!("no data header among {headers:?}"));
    assert_eq!(data["destination"], u64::from(TALKGROUP));
    assert_eq!(data["blocks_to_follow"], 2);

    // And the decoder evidence a row's explanations rank from: the family is the **decoder id**,
    // as it is for every trunking decoder (T-546), and the vocabulary is what maps it to a
    // service — so an explanation says which air interface said so.
    let families: Vec<Option<String>> = entries.iter().map(|e| e.family.clone()).collect();
    assert!(
        families.iter().any(|f| f.as_deref() == Some("dmr-tier2")),
        "no dmr-tier2 decoder evidence on any row: {families:?}"
    );
    let mapped = hk_pipeline::family::lookup("dmr-tier2").expect("dmr-tier2 is in the vocabulary");
    assert_eq!(mapped.service, Some("public-safety"));
    assert!(mapped.confidence >= 0.9);

    // The headers also left the run as a stream another program can consume, each one marked
    // with the check that passed and carrying no content.
    let bytes = sink.0.lock().unwrap().clone();
    let records: Vec<serde_json::Value> = frames(&bytes)
        .into_iter()
        .filter(|v| v["type"] == "message")
        .collect();
    assert!(!records.is_empty(), "no DMR headers reached the stream");
    let lc = records
        .iter()
        .find(|r| r["frame_model"] == "dmr-tier2/voice-lc-header")
        .unwrap_or_else(|| panic!("no voice LC header on the stream: {records:?}"));
    assert_eq!(
        lc["crc_status"], "valid",
        "a published header is a checked one"
    );
    assert_eq!(lc["metadata"]["destination"], u64::from(TALKGROUP));
    assert_eq!(lc["metadata"]["source"], u64::from(SOURCE_UNIT));
    assert_eq!(lc["content_class"], "metadata-only");
    assert!(lc.get("content").is_none_or(|c| c.is_null()), "{lc:?}");
    assert!(
        records
            .iter()
            .any(|r| r["frame_model"] == "dmr-tier2/data-header"),
        "no data header on the stream"
    );
}
