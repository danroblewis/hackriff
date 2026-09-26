//! T-981: a front-end overload is a **front-end event**, never a signal.
//!
//! The explorer (2026-09-25, live HackRF, amp on, 25–30 dB pager bursts) saw one-row stripes
//! across the whole tuned window drawn on the canvas as energy, with no overload flag anywhere and
//! nothing in `/api/status`. This drives the same shape blind through the **mock SDR** (the device
//! interface, never a file fed to the pipeline): a steady tone in noise, and at [`BURST_AT_S`] one
//! display row's worth of broadband energy that saturates the 8-bit ADC — the whole-span energy
//! step coinciding with clipping.
//!
//! Asserted on what the run publishes and stores, not on a helper:
//!
//! 1. **The row is flagged** in the spectrum stream's frame metadata: the published record(s) over
//!    the burst carry `CLIPPED` and `FRONTEND_EVENT`, and the rows away from it carry neither.
//! 2. **Status counts it**: `/api/status`'s `frontend` block (the same JSON the run summary holds)
//!    counts the clipped rows and one front-end event, whose extent is the tuned window and whose
//!    time is the burst's.
//! 3. **No detection**: no stored Detection spans the whole window over the burst — the stripe is
//!    suppressed, and counted as suppressed.
//! 4. **The real signal survives**: the tone is still detected (clipping is provenance, never a
//!    reason to drop a narrow emission).

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};

use common::*;
use hk_core::{Gains, MockOptions, MockSdrDriver, Pacing, SourceControl};
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_model::{Detection, FreqRange, Region, TimeRange, Timestamp};
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, open_mock_replay};
use hk_stream::{Declared, Record, StreamKind, StreamReader};

const FS: f64 = 2e6;
const CENTER: f64 = 915e6;
const RUN_S: f64 = 4.0;
/// The overload burst starts here (recording time, s) …
const BURST_AT_S: f64 = 2.0;
/// … and lasts one display row (25 rows/s).
const BURST_S: f64 = 0.04;
/// The tone's offset from the centre.
const TONE_OFFSET: f64 = 50e3;
/// `RecordFlags` bits on the wire (`docs/stream-contract.md`): bit 5 `CLIPPED`, bit 6
/// `FRONTEND_EVENT`. Raw, so the assertion reads the frame metadata a client reads.
const CLIPPED: u8 = 1 << 5;
const FRONTEND_EVENT: u8 = 1 << 6;
/// The recording's start (its capture `datetime`).
const START_S: f64 = 1_789_300_800.0; // 2026-09-13T12:00:00Z

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

/// A ci8 recording: a +50 kHz tone (amplitude 40) in noise, with [`BURST_S`] of broadband energy
/// at [`BURST_AT_S`] that drives both components onto the rails most of the time.
fn overload_recording(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let n = (RUN_S * FS) as usize;
    let (b0, b1) = (
        (BURST_AT_S * FS) as usize,
        ((BURST_AT_S + BURST_S) * FS) as usize,
    );
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut uni = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 40) as f64 / (1u64 << 24) as f64 - 0.5
    };
    let mut data = Vec::with_capacity(2 * n);
    for i in 0..n {
        let ph = 2.0 * std::f64::consts::PI * TONE_OFFSET * i as f64 / FS;
        let (mut re, mut im) = (
            40.0 * ph.cos() + uni() * 12.0,
            40.0 * ph.sin() + uni() * 12.0,
        );
        if (b0..b1).contains(&i) {
            // White, and ~4× full scale: a saturated front end, flat across the whole window.
            re += uni() * 1000.0;
            im += uni() * 1000.0;
        }
        data.push(re.round().clamp(-128.0, 127.0) as i8 as u8);
        data.push(im.round().clamp(-128.0, 127.0) as i8 as u8);
    }
    std::fs::write(dir.join("overload.sigmf-data"), data).unwrap();
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(FS);
    meta.captures.push(Capture {
        sample_start: 0,
        frequency: Some(CENTER),
        datetime: Some("2026-09-13T12:00:00Z".into()),
        provenance: None,
        clip_count: None,
        extra: serde_json::Map::new(),
    });
    let path = dir.join("overload.sigmf-meta");
    meta.write(&path).unwrap();
    path
}

fn detections(dir: &std::path::Path) -> Vec<Detection> {
    let ever = TimeRange::new(
        Timestamp::from_unix_nanos(0),
        Timestamp::from_unix_nanos(i64::MAX / 2),
    );
    repo(dir)
        .detections_in_region(&Region::new(FreqRange::new(0.0, 7e9), ever))
        .unwrap()
}

#[test]
fn an_overload_row_is_a_front_end_event_flagged_counted_and_never_a_detection() {
    let dir = TempDir::new("t981-overload");
    let rec = overload_recording(&dir.0.join("src"));
    let reference = open_mock_replay(&rec, Pacing::Unpaced, hk_core::MockEnd::Stop).unwrap();
    let driver = MockSdrDriver::new(
        &rec,
        MockOptions {
            block_len: hk_pipeline::replay_block_len(FS),
            pacing: Pacing::Unpaced,
            ..MockOptions::default()
        },
    )
    .unwrap();
    let source = driver.open_mock(&driver.default_request()).unwrap();
    let control = driver.last_control().unwrap();
    // The working gain (the scheduler's default): the recording is served as recorded, so the
    // only clipping in the run is the burst's.
    control
        .set_gains(&Gains {
            lna_db: 24.0,
            vga_db: 20.0,
            amp_on: false,
        })
        .unwrap();
    let info = SourceInfo {
        sample_rate_hz: FS,
        center_hz: CENTER,
        start_time: source.start_time(),
    };
    let plan = hk_pipeline::replay_plan(CENTER, FS, info.start_time);
    let data = dir.0.join("data");
    let mut cfg = PipelineConfig::new(&data, plan).unwrap();
    cfg.source_class = reference.class;
    cfg.lossless = true;
    drop(reference);
    let spectrum: Arc<Mutex<Vec<Buf>>> = Arc::default();
    let taps = Arc::clone(&spectrum);
    cfg.stream_sink = Some(Arc::new(move |h, handle| {
        if h.kind == StreamKind::Spectrum {
            let buf = Buf::default();
            handle
                .subscribe("t981", Declared::local(buf.clone()), Box::new(|_| {}))
                .unwrap();
            taps.lock().unwrap().push(buf);
        }
    }));
    let handle = Pipeline::start(
        cfg,
        Box::new(source),
        info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let (summary, stopped) = wait_guarded(handle, std::time::Duration::from_secs(120));
    assert!(!stopped, "the run finished on its own");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    assert!(
        control.mock_stats().clipped_components > 0,
        "the burst saturates the ADC"
    );

    // Where the burst is on the run's capture clock.
    let t0 = START_S + BURST_AT_S;
    let t1 = t0 + BURST_S;
    let row_s = 1.0 / 25.0;

    // (1) The frame metadata: every published spectrum row, with its time and flags.
    let mut rows: Vec<(f64, u8)> = Vec::new();
    for b in spectrum.lock().unwrap().iter() {
        let bytes = b.0.lock().unwrap().clone();
        let mut reader = StreamReader::new(&bytes[..]);
        if reader.read_header().is_err() {
            continue;
        }
        while let Ok(Some(record)) = reader.next_record() {
            if let Record::Binary(r) = record {
                rows.push((r.header.t.as_unix_nanos() as f64 / 1e9, r.header.flags.0));
            }
        }
    }
    assert!(
        rows.len() > 50,
        "the spectrum stream published rows: {}",
        rows.len()
    );
    let flagged: Vec<_> = rows.iter().filter(|(_, f)| f & CLIPPED != 0).collect();
    eprintln!("rows {} clipped-flagged {flagged:?}", rows.len());
    assert!(
        !flagged.is_empty(),
        "a row over the burst carries CLIPPED in its frame metadata"
    );
    for &&(t, f) in &flagged {
        assert!(
            t > t0 - 2.0 * row_s && t < t1 + row_s,
            "only the rows over the burst are flagged CLIPPED (row at {t}, burst {t0}..{t1})"
        );
        assert!(
            f & FRONTEND_EVENT != 0,
            "a clipped whole-span step is a front-end event (row at {t})"
        );
    }
    assert!(
        flagged.len() <= 3,
        "one ~row-long burst flags at most the rows it touches: {}",
        flagged.len()
    );

    // (2) Status: the `frontend` block of `/api/status` (the run summary's counters).
    let fe = summary
        .counters
        .get("frontend")
        .cloned()
        .unwrap_or_default();
    eprintln!("frontend {fe:#}");
    assert!(summary.counter("/frontend/rows") > 50, "rows measured");
    assert_eq!(
        summary.counter("/frontend/clipped_rows"),
        flagged.len() as u64,
        "status counts exactly the flagged rows"
    );
    assert_eq!(
        summary.counter("/frontend/events"),
        1,
        "one front-end event"
    );
    assert!(summary.counter("/frontend/clipped_samples") > 0);
    let last = &fe["last_event"];
    let (e0, e1) = (
        last["t0"].as_f64().unwrap_or(f64::NAN),
        last["t1"].as_f64().unwrap_or(f64::NAN),
    );
    assert!(
        e0 < t1 && e1 > t0,
        "the event covers the burst: event {e0}..{e1}, burst {t0}..{t1}"
    );
    assert!(e1 - e0 < 4.0 * row_s, "and not much more: {}", e1 - e0);
    assert_eq!(last["f_lo_hz"].as_f64(), Some(CENTER - FS / 2.0));
    assert_eq!(last["f_hi_hz"].as_f64(), Some(CENTER + FS / 2.0));
    assert!(
        last["adc_peak_max"].as_f64().unwrap_or(0.0) >= 0.99,
        "{last}"
    );
    assert!(
        last["step_db_max"].as_f64().unwrap_or(0.0) >= 6.0,
        "a whole-span step over the level before it: {last}"
    );

    // (3) No detection spans the window over the burst.
    let dets = detections(&data);
    let over_burst = |d: &Detection| {
        let (a, b) = (
            d.time.start.as_unix_nanos() as f64 / 1e9,
            d.time.end.as_unix_nanos() as f64 / 1e9,
        );
        a < t1 + row_s && b > t0 - row_s
    };
    let stripe: Vec<_> = dets
        .iter()
        .filter(|d| over_burst(d) && (d.flags.impulsive || d.obw_hz >= 0.5 * FS))
        .collect();
    eprintln!(
        "{} detections; over the burst and whole-span: {:?}",
        dets.len(),
        stripe
            .iter()
            .map(|d| (d.f_center_hz, d.obw_hz, d.flags.impulsive, d.flags.clipped))
            .collect::<Vec<_>>()
    );
    assert!(
        stripe.is_empty(),
        "the overload stripe is not stored as a detection"
    );
    assert!(
        summary.counter("/frontend/suppressed_detections") >= 1,
        "and its suppression is counted"
    );

    // (4) The real emission is still detected.
    assert!(
        dets.iter()
            .any(|d| (d.f_center_hz - (CENTER + TONE_OFFSET)).abs() < 20e3),
        "the tone is detected blind"
    );
}
