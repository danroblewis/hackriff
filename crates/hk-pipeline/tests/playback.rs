//! T-463 (MPLAY; AWARE-011, AWARE-042): historical playback — play from a chosen past time, with
//! play and pause, and "now" moves forward over recorded history as if it were live.
//!
//! One run records an NBFM carrier (a 1 kHz tone, 3 kHz deviation, 150 kHz above the tuned
//! centre) **through the mock SDR device interface** with the IQ ring on, and stores its analysis
//! (blind detections, the inventory) as it goes. Then, with the run over, playback:
//!
//! - **plays from a chosen time**: the first audio record is stamped at that capture time, and
//!   every record's time is on the one shared capture-time axis;
//! - **re-runs demod** from the raw IQ the ring holds: the mode is estimated (`nbfm`, no mode
//!   given), and the audio carries the 1 kHz tone that exists only in the IQ;
//! - **pauses and plays**: a paused playhead does not move and no audio is produced while it is
//!   paused; playing resumes from where it stopped;
//! - **replays the analysis unchanged**: every `Detection` and every inventory entry reads back
//!   identical after playback, and playback stored nothing (no recording row, no detection);
//! - **respects the IQ horizon**: a position before the ring holds IQ is refused up front with
//!   the reason and the next time IQ exists (`no-iq`, waterfall-only), and a playhead that runs
//!   past the ring's end says `iq: false` and carries no audio past it.

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{TempDir, inventory, repo};
use hk_core::{MockEnd, MockOptions, MockSdrDriver, Source};
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_model::{FreqRange, InventoryQuery, Region, TimeRange, Timestamp};
use hk_pipeline::class::window_class;
use hk_pipeline::playback::{PlaybackConfig, PlaybackService, RunIq};
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, replay_plan};
use hk_store::iqbuffer::IqBufferConfig;
use hk_stream::{Declared, OpenRequest, OpenedStream, Record, StreamOpener, StreamReader};

const AWARE_011: &str = "AWARE-011";
const FS: f64 = 1.0e6;
const CENTER_HZ: f64 = 100.0e6;
const OFFSET_HZ: f64 = 150e3;
const SECS: f64 = 6.0;
const TONE_HZ: f64 = 1_000.0;
const DEV_HZ: f64 = 3_000.0;
const LIMIT: Duration = Duration::from_secs(60);

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

/// What a consumer received: audio records `(t_ns, flags_discontinuity, pcm)` and status objects.
struct Received {
    audio: Vec<(i64, bool, Vec<f32>)>,
    status: Vec<serde_json::Value>,
}

impl Buf {
    fn read(&self) -> Received {
        let bytes = self.0.lock().unwrap().clone();
        let mut reader = StreamReader::new(std::io::Cursor::new(bytes));
        let (mut audio, mut status) = (Vec::new(), Vec::new());
        while let Ok(Some(r)) = reader.next_record() {
            match r {
                Record::Binary(b) => {
                    let pcm = b
                        .payload
                        .chunks_exact(2)
                        .map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32767.0)
                        .collect();
                    audio.push((
                        b.header.t.as_unix_nanos(),
                        b.header
                            .flags
                            .contains(hk_stream::RecordFlags::DISCONTINUITY),
                        pcm,
                    ));
                }
                Record::Unknown(frame) => {
                    if let Some((_, v)) = hk_stream::record::parse_status_record(&frame) {
                        status.push(v);
                    }
                }
                _ => {}
            }
        }
        Received { audio, status }
    }
}

/// An NBFM tone recording, ci8, with a capture datetime.
fn nbfm_recording(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let n = (SECS * FS) as usize;
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut noise = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        ((state >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 6.0
    };
    let tau = std::f64::consts::TAU;
    let mut data = Vec::with_capacity(2 * n);
    for k in 0..n {
        let t = k as f64 / FS;
        let ph = tau * OFFSET_HZ * t + DEV_HZ / TONE_HZ * (tau * TONE_HZ * t).sin();
        data.push((50.0 * ph.cos() + noise()).round().clamp(-128.0, 127.0) as i8 as u8);
        data.push((50.0 * ph.sin() + noise()).round().clamp(-128.0, 127.0) as i8 as u8);
    }
    std::fs::write(dir.join("nbfm.sigmf-data"), data).unwrap();
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(FS);
    meta.captures.push(Capture {
        sample_start: 0,
        frequency: Some(CENTER_HZ),
        datetime: Some("2026-09-13T12:00:00Z".into()),
        provenance: None,
        clip_count: None,
        extra: serde_json::Map::new(),
    });
    let path = dir.join("nbfm.sigmf-meta");
    meta.write(&path).unwrap();
    path
}

/// Tone power at `f` over `x` (48 kS/s), Goertzel.
fn power_at(x: &[f32], f: f64) -> f64 {
    let w = std::f64::consts::TAU * f / 48_000.0;
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &v in x {
        let s0 = f64::from(v) + 2.0 * w.cos() * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2) / x.len().max(1) as f64
}

fn wait(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + LIMIT;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn request(params: &[(&str, String)]) -> OpenRequest {
    OpenRequest {
        params: params
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect(),
        peer: "t463".into(),
    }
}

fn subscribe(opened: &OpenedStream) -> Buf {
    let buf = Buf::default();
    opened
        .handle
        .subscribe("t463", Declared::local(buf.clone()), Box::new(|_| {}))
        .unwrap();
    buf
}

/// The analysis records, as the `docs/07` objects: every detection and every inventory entry.
fn analysis(dir: &std::path::Path) -> (Vec<hk_model::Detection>, Vec<hk_model::InventoryEntry>) {
    let repo = repo(dir);
    let all = Region::new(
        FreqRange::new(0.0, 10e9),
        TimeRange::new(
            Timestamp::from_unix_nanos(0),
            Timestamp::from_unix_nanos(i64::MAX / 2),
        ),
    );
    let mut dets = repo.detections_in_region(&all).unwrap();
    dets.sort_by_key(|d| d.id.to_string());
    (dets, inventory(&repo, InventoryQuery::default()))
}

#[test]
fn aware_011_playback_replays_analysis_unchanged_and_reruns_demod_from_raw_iq() {
    let dir = TempDir::new("t463-playback");
    let meta = nbfm_recording(&dir.0.join("rec"));
    // ---- Record: through the mock SDR device interface, IQ ring on, analysis stored. ----------
    let driver = MockSdrDriver::new(
        &meta,
        MockOptions {
            end: MockEnd::Stop,
            block_len: 16_384,
            ..MockOptions::default()
        },
    )
    .unwrap();
    let source = driver.open_mock(&driver.default_request()).unwrap();
    let info = SourceInfo {
        sample_rate_hz: source.recording().sample_rate_hz,
        center_hz: source.recording().center_hz,
        start_time: source.start_time(),
    };
    let mut cfg = PipelineConfig::new(
        &dir.0,
        replay_plan(info.center_hz, info.sample_rate_hz, info.start_time),
    )
    .unwrap();
    cfg.source_class = window_class(CENTER_HZ, FS);
    cfg.live_window_class = true;
    cfg.lossless = source.pausable();
    cfg.iq_buffer = IqBufferConfig {
        enabled: Some(true),
        retention_s: 600.0,
        max_bytes: Some(64 << 20),
        min_free_bytes: Some(0),
        ..IqBufferConfig::default()
    };
    let handle = Pipeline::start(
        cfg,
        Box::new(source),
        info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let ring = handle.iq_buffer();
    assert!(ring.wait_allocated(LIMIT) && ring.enabled());
    let summary = handle.wait().unwrap();
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    let status = ring.status(None, None, 10);
    let (t0, t1) = (
        (status.t0.expect("ring holds IQ") * 1e9).round() as i64,
        (status.t1.unwrap() * 1e9).round() as i64,
    );
    assert_eq!(
        t0,
        info.start_time.as_unix_nanos(),
        "the ring holds the run"
    );
    assert!(t1 - t0 >= 5_900_000_000, "the whole run is in the ring");

    let before = analysis(&dir.0);
    let fc = CENTER_HZ + OFFSET_HZ;
    assert!(
        before
            .0
            .iter()
            .any(|d| (d.freq().lo_hz..=d.freq().hi_hz).contains(&fc)),
        "[{AWARE_011}] the run detected the carrier blindly: {:?}",
        before
            .0
            .iter()
            .map(|d| (d.freq().lo_hz, d.freq().hi_hz))
            .collect::<Vec<_>>()
    );

    let service = PlaybackService::new(
        Arc::new(RunIq::new(
            Arc::clone(&ring),
            Some((dir.0.join("hackriff.db"), dir.0.clone())),
        )),
        PlaybackConfig::default(),
    );
    let band = |p: &mut Vec<(&str, String)>| {
        p.push(("f_lo", format!("{}", fc - 8e3)));
        p.push(("f_hi", format!("{}", fc + 8e3)));
    };

    // ---- The IQ horizon is decided up front, never discovered. --------------------------------
    let mut p = vec![("t", format!("{}", (t0 - 60_000_000_000) as f64 / 1e9))];
    band(&mut p);
    let refused = service
        .open(&request(&p))
        .err()
        .expect("before the ring: refused");
    assert_eq!((refused.status, refused.code.as_str()), (409, "no-iq"));
    assert!(
        refused.reason.contains("waterfall-only") && refused.reason.contains("next exists"),
        "the refusal names the horizon and when IQ resumes: {}",
        refused.reason
    );

    // ---- Play from a chosen time (2 s in), at 2x. ----------------------------------------------
    let start = t0 + 2_000_000_000;
    let mut p = vec![
        ("t", format!("{}", start as f64 / 1e9)),
        ("speed", "2".into()),
    ];
    band(&mut p);
    let opened = service.open(&request(&p)).expect("play from 2 s");
    let audio = opened.header.audio.clone().unwrap();
    assert_eq!(
        audio.mode, "nbfm",
        "[{AWARE_011}] mode estimated from the IQ"
    );
    let got = subscribe(&opened);
    service.playhead().play().unwrap();
    let head = service.playhead();
    wait("the playhead at 3 s", || {
        head.position_ns().unwrap() >= t0 + 3_000_000_000
    });
    wait("audio", || got.read().audio.len() >= 20);

    // Pause: the playhead freezes and no audio is produced while it is paused.
    let paused = head.pause();
    std::thread::sleep(Duration::from_millis(150));
    let frames_paused = got.read().audio.len();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        head.position_ns(),
        paused.position_ns,
        "a paused playhead holds"
    );
    assert_eq!(
        got.read().audio.len(),
        frames_paused,
        "no audio while paused"
    );

    // Play again: it resumes from where it stopped.
    let resumed = head.play().unwrap();
    assert_eq!(resumed.position_ns, paused.position_ns);
    let paused_at = paused.position_ns.unwrap();
    wait("a second after resuming", || {
        head.position_ns().unwrap() >= paused_at + 1_000_000_000
    });
    wait("audio after resuming", || {
        got.read()
            .audio
            .iter()
            .any(|(t, ..)| *t > paused_at + 200_000_000)
    });
    drop(opened);

    let r = got.read();
    let first = r.audio[0].0;
    assert!(
        (start..start + 100_000_000).contains(&first),
        "[{AWARE_011}] audio starts at the chosen capture time: first {first}, chose {start}"
    );
    assert!(
        r.audio.windows(2).all(|w| w[0].0 < w[1].0),
        "audio times are monotone capture time"
    );
    // Demod re-ran from raw IQ: the 1 kHz tone is only in the IQ.
    let pcm: Vec<f32> = r.audio.iter().skip(5).flat_map(|a| a.2.clone()).collect();
    let tone = power_at(&pcm, TONE_HZ);
    for other in [400.0, 2_500.0, 3_700.0] {
        assert!(
            tone > 30.0 * power_at(&pcm, other),
            "[{AWARE_011}] the demodulated audio carries the recorded 1 kHz tone ({tone} vs \
             {other} Hz {})",
            power_at(&pcm, other)
        );
    }
    assert!(
        r.status.iter().all(|s| s["analysis"] == "recorded"),
        "the stream says analysis is the recorded one"
    );
    assert!(
        r.status
            .iter()
            .any(|s| s["iq"] == true && s["iq_source"] == "ring"),
        "{:?}",
        r.status
    );

    // ---- Past the ring's end: iq: false, and no audio beyond it. ------------------------------
    let near_end = t1 - 300_000_000;
    let mut p = vec![
        ("t", format!("{}", near_end as f64 / 1e9)),
        ("speed", "4".into()),
    ];
    band(&mut p);
    let opened = service.open(&request(&p)).expect("play near the end");
    let tail = subscribe(&opened);
    wait("the horizon crossed", || {
        tail.read().status.iter().any(|s| s["iq"] == false)
    });
    drop(opened);
    let r = tail.read();
    assert!(
        r.audio.iter().all(|(t, ..)| *t < t1),
        "no audio past the IQ horizon"
    );
    let crossed = r.status.iter().find(|s| s["iq"] == false).unwrap();
    assert_eq!(crossed["iq_source"], "none");

    // ---- Analysis replayed unchanged: the records are what the run wrote. ---------------------
    let after = analysis(&dir.0);
    assert_eq!(
        before.0, after.0,
        "[{AWARE_011}] detections are immutable records"
    );
    assert_eq!(
        before.1, after.1,
        "[{AWARE_011}] the inventory is unchanged"
    );
    let recordings = std::fs::read_dir(dir.0.join("recordings"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(recordings, 0, "playback stored no clip");
    eprintln!(
        "[{AWARE_011}] playback: {} detections unchanged, tone {tone:.3e}",
        after.0.len()
    );
}

// ---- Review finding (T-463 fix 1): audio stays on the air's own capture times across windows ----

/// A fake IQ archive over one in-memory NBFM recording, serving it in short windows exactly as
/// the ring does (first sample at or after the asked time), and logging every `open_at`.
struct Windows {
    iq: Vec<u8>,
    t0_ns: i64,
    window: usize,
    opens: Mutex<Vec<i64>>,
}

const WFS: f64 = 250e3; // 4000 ns a sample: exact integer sample times
const WPERIOD_NS: i64 = 4_000;
const WCENTER: f64 = 100e6;
const WOFFSET: f64 = 50e3;

impl Windows {
    fn new(secs: f64, window_s: f64) -> Self {
        let n = (secs * WFS) as usize;
        let tau = std::f64::consts::TAU;
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut iq = Vec::with_capacity(2 * n);
        for k in 0..n {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let nz = ((state >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 4.0;
            let t = k as f64 / WFS;
            let ph = tau * WOFFSET * t + DEV_HZ / TONE_HZ * (tau * TONE_HZ * t).sin();
            iq.push((50.0 * ph.cos() + nz).round() as i8 as u8);
            iq.push((50.0 * ph.sin() - nz).round() as i8 as u8);
        }
        Self {
            iq,
            t0_ns: 1_789_300_800_000_000_000, // 2026-09-13T12:00:00Z
            window: (window_s * WFS) as usize,
            opens: Mutex::new(Vec::new()),
        }
    }
}

impl hk_pipeline::playback::IqArchive for Windows {
    fn open_at(
        &self,
        t_ns: i64,
        _band: (f64, f64),
    ) -> Result<hk_pipeline::playback::IqWindow, hk_pipeline::playback::NoIq> {
        self.opens.lock().unwrap().push(t_ns);
        let n = self.iq.len() / 2;
        let k0 = ((t_ns - self.t0_ns).max(0) + WPERIOD_NS - 1) / WPERIOD_NS;
        let k0 = k0 as usize;
        if k0 >= n {
            return Err(hk_pipeline::playback::NoIq {
                reason: "past the end: waterfall-only".into(),
                next_iq_ns: None,
            });
        }
        let k1 = (k0 + self.window).min(n);
        let at = k0 as i64 * WPERIOD_NS; // < 60 s after 12:00:00
        let mut meta = SigmfMeta::new(Datatype::Ci8);
        meta.global.sample_rate = Some(WFS);
        meta.captures.push(Capture {
            sample_start: 0,
            frequency: Some(WCENTER),
            datetime: Some(format!(
                "2026-09-13T12:00:{:02}.{:09}Z",
                at / 1_000_000_000,
                at % 1_000_000_000
            )),
            provenance: None,
            clip_count: None,
            extra: serde_json::Map::new(),
        });
        let data = self.iq[2 * k0..2 * k1].to_vec();
        let source = hk_core::SigmfReplaySource::from_reader(
            meta,
            std::io::Cursor::new(data),
            hk_core::ReplayOptions::default(),
        )
        .unwrap();
        Ok(hk_pipeline::playback::IqWindow {
            source: Box::new(source),
            content_class: hk_model::ContentClass::Unrestricted,
            origin: hk_pipeline::playback::IqOrigin::Ring,
            t1_ns: self.t0_ns + k1 as i64 * WPERIOD_NS,
        })
    }
}

/// With 50 ms windows at 16x, the playhead runs ahead of the demodulator by a block's demod
/// time on every window. Each next window must still open where the last block ended (never at
/// the playhead), and every audio record must sit exactly on the air's own timeline: one
/// unbroken run of 20 ms records from the chosen start, no false DISCONTINUITY at the window
/// seams, and exactly as much audio as the IQ between the first and last window.
#[test]
fn aware_011_playback_audio_stays_on_capture_time_across_windows() {
    let archive = Arc::new(Windows::new(3.0, 0.05));
    let service = PlaybackService::new(
        Arc::clone(&archive) as Arc<dyn hk_pipeline::playback::IqArchive>,
        PlaybackConfig::default(),
    );
    let t0 = archive.t0_ns;
    let start = t0 + 200_000_000;
    let fc = WCENTER + WOFFSET;
    let opened = service
        .open(&request(&[
            ("t", format!("{}", start as f64 / 1e9)),
            ("speed", "16".into()),
            ("f_lo", format!("{}", fc - 8e3)),
            ("f_hi", format!("{}", fc + 8e3)),
        ]))
        .expect("open");
    let got = subscribe(&opened);
    service.playhead().play().unwrap();
    // Until the demodulator has crossed the IQ's end (2.8 s of IQ at 16x).
    wait("the IQ horizon", || {
        got.read().status.iter().any(|s| s["iq"] == false)
    });
    drop(opened);
    let r = got.read();
    let opens = archive.opens.lock().unwrap().clone();

    // Every window after the first opens where the last one ended (within half a sample). The
    // last open, at the IQ's end, is the horizon (answered no-IQ), not a window. `opens[0]` is
    // the opener's probe; `opens[1]` is where the playhead stood when demod began.
    let windows: Vec<i64> = opens
        .iter()
        .copied()
        .take_while(|&t| t < t0 + 3_000_000_000 - WPERIOD_NS)
        .collect();
    assert!(windows.len() > 40, "{} windows", windows.len());
    for (k, pair) in windows.windows(2).enumerate().skip(1) {
        let step = pair[1] - pair[0];
        assert!(
            (step - 50_000_000).abs() <= WPERIOD_NS,
            "[{AWARE_011}] window {k} opened {step} ns after the previous one, not 50 ms on: the \
             IQ between them was skipped (opens: {:?})",
            &windows[..windows.len().min(k + 3)]
        );
    }

    // One unbroken run of audio from the chosen start, on the air's timeline.
    let audio = &r.audio;
    assert!(audio.len() > 100, "{} records", audio.len());
    // The first record is at or after the chosen start. It may be a little later: at 16x the
    // thread's first window opens wherever the playhead has reached, and the squelch withholds
    // frames while its level estimate settles. Its stamp is still the air's own time; the unbroken
    // 20 ms run and the end check below are what pin that.
    assert!(
        (start..start + 100_000_000).contains(&audio[0].0),
        "first record {} vs start {start}",
        audio[0].0
    );
    let breaks: Vec<usize> = (1..audio.len()).filter(|&i| audio[i].1).collect();
    assert!(
        breaks.is_empty(),
        "[{AWARE_011}] continuous IQ must not be flagged DISCONTINUITY at window seams: {breaks:?}"
    );
    for (i, w) in audio.windows(2).enumerate() {
        let dt = w[1].0 - w[0].0;
        assert!(
            (dt - 20_000_000).abs() <= 1,
            "[{AWARE_011}] record {i}->{} is {dt} ns apart, not 20 ms",
            i + 1
        );
    }
    // As much audio as IQ: the last record ends within one frame plus filter delay of the IQ end
    // (2.8 s of IQ after the start). A skipped window seam would leave it short.
    let end = audio.last().unwrap().0 + 20_000_000;
    let iq_end = t0 + 3_000_000_000;
    assert!(
        (iq_end - 45_000_000..=iq_end).contains(&end),
        "[{AWARE_011}] audio ends at {} ms before the IQ does",
        (iq_end - end) / 1_000_000
    );
}
