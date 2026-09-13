//! T-045 frequency axis (SIGNAL-062, SPACE-050): a spectrum row's bins map to the right RF
//! frequencies from the pipeline's STFT, through the stream header, to the UI's axis function.
//!
//! - **Synthetic tone** at centre + 500 kHz (ci8, 100.8 MHz, 2.4 Msps) through the composed
//!   pipeline (the `hk-pipeline` spectrum reader behind `hk replay --serve` and `hackriffd`). The
//!   header centre is the capture's `core:frequency`, the span is the sample rate, and the strongest
//!   bin, mapped by [`StreamHeader::spectrum_bin_hz`], is within one bin of the tone.
//! - **Real fixture** `fm_100p8M_2p4M_l32g30a1_t1p5_5s`: header centre 100.8 MHz, and the
//!   strongest bin maps to the 101.3 MHz station within one bin. Skips when the LFS data is not
//!   fetched (`HK_REQUIRE_FIXTURES=1` fails instead).
//! - **Into the UI.** Header geometry and peak bins are pinned in
//!   `ui/test/spectrum_axis.golden.json`. `ui/test/axis.test.ts` maps those numbers through
//!   `ui/src/axis.ts` (bin → Hz → pixel → pointer readout, and the texture column drawn there),
//!   so both halves assert on the same values. `HK_UPDATE_GOLDEN=1` rewrites the file.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hk_core::Pacing;
use hk_model::ContentClass;
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_pipeline::{Pipeline, PipelineConfig, TrackInventory, open_replay, replay_plan};
use hk_stream::{Declared, FrameDecoder, MAX_FRAME_LEN, StreamHeader, StreamKind};
use serde_json::{Value, json};

const T_045: &str = "T-045";
const SIGNAL_062: &str = "SIGNAL-062";
const FC: f64 = 100.8e6;
const FS: f64 = 2.4e6;
const TONE_OFFSET_HZ: f64 = 500e3;
const FM_FIXTURE: &str = "fm_100p8M_2p4M_l32g30a1_t1p5_5s";
const STATION_HZ: f64 = 101.3e6;

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

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hk-e2e-t045-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
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

/// 0.5 s of a ci8 tone at `FC + TONE_OFFSET_HZ` (amplitude 50 codes) in ±6-code noise.
fn tone_recording(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let n = (0.5 * FS) as usize;
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut noise = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        ((state >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 12.0
    };
    let mut data = Vec::with_capacity(2 * n);
    for i in 0..n {
        let ph = 2.0 * std::f64::consts::PI * TONE_OFFSET_HZ * i as f64 / FS;
        let re = (50.0 * ph.cos() + noise()).round().clamp(-128.0, 127.0) as i8;
        let im = (50.0 * ph.sin() + noise()).round().clamp(-128.0, 127.0) as i8;
        data.push(re as u8);
        data.push(im as u8);
    }
    std::fs::write(dir.join("tone.sigmf-data"), data).unwrap();
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(FS);
    meta.captures.push(Capture {
        sample_start: 0,
        frequency: Some(FC),
        datetime: Some("2026-09-13T12:00:00Z".into()),
        provenance: None,
        clip_count: None,
        extra: serde_json::Map::new(),
    });
    let path = dir.join("tone.sigmf-meta");
    meta.write(&path).unwrap();
    path
}

/// A real fixture whose LFS data is present here or in an ancestor checkout (a worktree may hold
/// only pointers).
fn real_fixture(name: &str) -> Option<PathBuf> {
    let rel = Path::new("fixtures/hackrf/2026-09-13").join(format!("{name}.sigmf-meta"));
    let mut dir = Some(hk_e2e::paths::repo_root());
    while let Some(d) = dir {
        let meta = d.join(&rel);
        let fetched = std::fs::read(meta.with_extension("sigmf-data"))
            .is_ok_and(|b| b.len() > 4096 && !b.starts_with(b"version https://git-lfs"));
        if fetched {
            return Some(meta);
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    if std::env::var("HK_REQUIRE_FIXTURES").is_ok_and(|v| v == "1") {
        panic!("{name}: fixture data not fetched (git lfs pull)");
    }
    eprintln!("SKIP {name}: fixture data is not fetched (git lfs pull)");
    None
}

/// Splits a consumer's framed bytes into the header and the delivered data rows.
fn decode(bytes: &[u8]) -> (Option<StreamHeader>, Vec<Vec<f32>>) {
    let mut dec = FrameDecoder::new(MAX_FRAME_LEN);
    dec.push(bytes);
    let mut header = None;
    let mut rows = Vec::new();
    while let Ok(Some(frame)) = dec.next_frame() {
        if header.is_none() {
            header = Some(StreamHeader::from_json_bytes(frame).expect("header frame"));
            continue;
        }
        // 32-byte binary record header (type 1 = data, flags bit 0 = gated), then f32 LE values.
        if frame.len() < 32 || frame[0] != 1 || frame[1] & 1 != 0 {
            continue;
        }
        rows.push(
            frame[32..]
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
        );
    }
    (header, rows)
}

/// Runs `meta` through the composed pipeline (unpaced, lossless) and returns the spectrum
/// stream's header and data rows as a local consumer received them.
fn spectrum_rows(meta: &Path, dir: &Path) -> (StreamHeader, Vec<Vec<f32>>) {
    let replay = open_replay(meta, Pacing::Unpaced, false).unwrap();
    let info = replay.info;
    let plan = replay_plan(info.center_hz, info.sample_rate_hz, info.start_time);
    let mut cfg = PipelineConfig::new(dir, plan).unwrap();
    cfg.source_class = replay.class;
    cfg.lossless = true;
    let consumer = Buf::default();
    let sink = consumer.clone();
    cfg.stream_sink = Some(Arc::new(move |h, handle| {
        if h.kind == StreamKind::Spectrum {
            handle
                .subscribe("t045-axis", Declared::local(sink.clone()), Box::new(|_| {}))
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
    let summary = handle.wait().unwrap();
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    let expected = summary.counter("/spectrum/rows") - summary.counter("/spectrum/rows_gated");
    // The consumer's writer thread may still be draining when the run returns.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (header, rows) = decode(&consumer.0.lock().unwrap());
        if (rows.len() as u64 >= expected && header.is_some()) || Instant::now() > deadline {
            return (header.expect("[T-045] no spectrum header"), rows);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Index of the strongest bin of the rows' mean linear power.
fn mean_peak(rows: &[Vec<f32>]) -> usize {
    let mut acc = vec![0f64; rows[0].len()];
    for r in rows {
        for (a, &v) in acc.iter_mut().zip(r) {
            *a += 10f64.powf(f64::from(v) / 10.0);
        }
    }
    acc.iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .unwrap()
        .0
}

/// Asserts the header geometry and the signal's bin, and checks (or with `HK_UPDATE_GOLDEN=1`
/// writes) its golden section.
fn check(section: &str, h: &StreamHeader, rows: &[Vec<f32>], signal_hz: f64) {
    assert_eq!(h.kind, StreamKind::Spectrum);
    assert_eq!(h.content_class, ContentClass::Unrestricted, "FM band prior");
    assert_eq!(
        h.center_hz,
        Some(FC),
        "[{T_045}] header centre is the capture's core:frequency"
    );
    assert_eq!(
        h.bandwidth_hz,
        Some(FS),
        "[{T_045}] header span is the rate"
    );
    let n = h.fft_size.expect("fft_size") as usize;
    assert!(rows.len() >= 5, "[{T_045}] {} rows", rows.len());
    assert!(rows.iter().all(|r| r.len() == n), "rows are fft_size long");
    assert_eq!(
        h.spectrum_bin_hz((n / 2) as u32),
        Some(FC),
        "DC is element N/2"
    );
    let df = FS / n as f64;
    let peak = mean_peak(rows);
    let f = h.spectrum_bin_hz(peak as u32).unwrap();
    eprintln!(
        "[{T_045}] {section}: peak bin {peak} of {n} -> {f:.1} Hz (signal {signal_hz} Hz, bin {df} Hz)"
    );
    assert!(
        (f - signal_hz).abs() <= df,
        "[{T_045}/{SIGNAL_062}] {section}: peak bin {peak} maps to {f} Hz, signal {signal_hz} Hz, bin {df} Hz"
    );
    golden(
        section,
        json!({
            "center_hz": FC,
            "bandwidth_hz": FS,
            "fft_size": n,
            "signal_hz": signal_hz,
            "peak_bin": peak,
        }),
    );
}

static GOLDEN_LOCK: Mutex<()> = Mutex::new(());

fn golden(section: &str, observed: Value) {
    let _guard = GOLDEN_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let path = hk_e2e::paths::repo_root().join("ui/test/spectrum_axis.golden.json");
    if std::env::var("HK_UPDATE_GOLDEN").is_ok_and(|v| v == "1") {
        let mut doc: Value = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_else(|| json!({}));
        doc["_about"] = json!(
            "T-045: spectrum stream geometry and the strongest row bin observed through the \
             composed pipeline. Written by tests/e2e/tests/spectrum_axis.rs \
             (HK_UPDATE_GOLDEN=1); read by ui/test/axis.test.ts."
        );
        doc[section] = observed;
        std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap() + "\n").unwrap();
        return;
    }
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("{}: {e} (HK_UPDATE_GOLDEN=1 writes it)", path.display()));
    let doc: Value = serde_json::from_slice(&bytes).unwrap();
    let want = &doc[section];
    for key in ["center_hz", "bandwidth_hz", "fft_size", "signal_hz"] {
        assert_eq!(
            want[key], observed[key],
            "[{T_045}] golden {section}.{key}: the UI test maps these numbers (HK_UPDATE_GOLDEN=1 rewrites)"
        );
    }
    let (w, o) = (want["peak_bin"].as_i64(), observed["peak_bin"].as_i64());
    assert!(
        matches!((w, o), (Some(w), Some(o)) if (w - o).abs() <= 1),
        "[{T_045}] golden {section}.peak_bin {w:?}, observed {o:?}"
    );
}

#[test]
fn t045_tone_at_center_plus_500_khz_maps_to_its_frequency_within_one_bin() {
    let dir = TempDir::new("tone");
    let meta = tone_recording(&dir.0.join("src"));
    let (h, rows) = spectrum_rows(&meta, &dir.0.join("run"));
    check("synthetic_tone", &h, &rows, FC + TONE_OFFSET_HZ);
}

#[test]
fn t045_signal_062_fixture_station_at_101_3_mhz_and_center_100_8_mhz() {
    let Some(meta) = real_fixture(FM_FIXTURE) else {
        return;
    };
    let dir = TempDir::new("fm");
    let (h, rows) = spectrum_rows(&meta, &dir.0);
    check("fm_fixture", &h, &rows, STATION_HZ);
}
