//! T-157: the rolling IQ capture buffer end to end **through the mock SDR device interface** (a
//! tone recording behind `MockSdrDriver`, retuned through the pipeline controller), never feeding
//! files into the pipeline.
//!
//! - An exact ns range at the recording's own tuning selects exactly 300 000 samples from index
//!   200 000, and they are the recording's bytes (the mock passes its own centre and rate through
//!   bit-exact), with stream indices and times on the sample clock.
//! - A retune away and back: two exact index-range clips each span a retune. Each piece at the
//!   recording's tuning is byte-identical to the source; each piece in the retuned window (rendered
//!   by the mock) is byte-identical to that stream range exported alone. Counts are exact, the
//!   settle gaps are explicit in `core:global_index`, and the ns form of a range selects the same
//!   samples. A `Recording` row is stored.
//! - A clip over `max_clip_bytes` and malformed ranges are refused before anything is written.

mod common;

use std::time::{Duration, Instant};

use common::{TempDir, tone_recording, wait_guarded};
use hk_core::{MockEnd, MockOptions, MockSdrDriver, Source};
use hk_model::RecordingKind;
use hk_model::sigmf::SigmfMeta;
use hk_pipeline::class::window_class;
use hk_pipeline::iqbuffer::{ClipCaptureInfo, ClipExported, ClipFailure, ClipRange, ClipRequest};
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, replay_plan};
use hk_store::iqbuffer::{IqBufferConfig, IqBufferStatus};

const FS: f64 = 1.0e6;
const CENTER_HZ: f64 = 100.0e6;
const OTHER_HZ: f64 = 100.2e6;
const LIMIT: Duration = Duration::from_secs(120);
const PIECE: u64 = 300_000;
const CLIP_CAP: u64 = 3 << 20;

fn wait_status(
    what: &str,
    status: impl Fn() -> IqBufferStatus,
    f: impl Fn(&IqBufferStatus) -> bool,
) -> IqBufferStatus {
    let deadline = Instant::now() + LIMIT;
    loop {
        let s = status();
        if f(&s) {
            return s;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {s:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The recording's ci8 bytes of stream indices `[g, g + n)` (the mock loops it).
fn source_bytes(recorded: &[u8], g: u64, n: u64) -> Vec<u8> {
    let len = recorded.len() as u64 / 2;
    (g..g + n)
        .flat_map(|i| {
            let j = (2 * (i % len)) as usize;
            [recorded[j], recorded[j + 1]]
        })
        .collect()
}

fn piece<'a>(data: &'a [u8], p: &ClipCaptureInfo) -> &'a [u8] {
    &data[(2 * p.sample_start) as usize..(2 * (p.sample_start + p.samples)) as usize]
}

fn spans(c: &ClipExported) -> Vec<(u64, u64, u64)> {
    c.captures
        .iter()
        .map(|p| (p.sample_start, p.global_index, p.samples))
        .collect()
}

#[test]
fn capture_buffer_clip_spans_a_retune_with_segments_and_exact_iq() {
    let dir = TempDir::new("t157-iqbuffer");
    let meta = tone_recording(&dir.0.join("rec"), "tone", FS, 4.0, CENTER_HZ, None);
    let recorded = std::fs::read(meta.with_extension("sigmf-data")).unwrap();
    let driver = MockSdrDriver::new(
        &meta,
        MockOptions {
            end: MockEnd::Loop,
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
    assert!(cfg.lossless, "an unpaced mock runs lossless");
    cfg.settings.chains = Some(Vec::new());
    // A lossless replay buffers only when forced (the default is off for recordings).
    cfg.iq_buffer = IqBufferConfig {
        enabled: Some(true),
        retention_s: 600.0,
        max_bytes: Some(1 << 30),
        min_free_bytes: Some(0),
        max_clip_bytes: CLIP_CAP,
        ..IqBufferConfig::default()
    };
    let recordings = dir.0.join("recordings");
    let handle = Pipeline::start(
        cfg,
        Box::new(source),
        info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let buffer = handle.iq_buffer();
    assert!(buffer.enabled());
    let status = || buffer.status(None, None, 1000);
    let export = |range| {
        buffer.export_clip(&ClipRequest {
            range,
            band: None,
            label: None,
        })
    };

    // --- Fixed tune: an exact ns range is the recording's bytes ----------------------------------
    let s = wait_status("1 s buffered", status, |s| s.samples >= FS as u64);
    let seg0 = s.segments[0].clone();
    assert_eq!(seg0.global_index, 0, "buffered from the stream start");
    assert_eq!(seg0.center_hz, CENTER_HZ);
    assert_eq!(seg0.sample_rate_hz, FS);
    assert_eq!(seg0.t0_ns, info.start_time.as_unix_nanos(), "sample clock");
    let (t0, t1) = (seg0.t0_ns + 200_000_000, seg0.t0_ns + 500_000_000);
    let fixed = buffer
        .export_clip(&ClipRequest {
            range: ClipRange::Time {
                t0_ns: t0,
                t1_ns: t1,
            },
            band: None,
            label: Some("fixed tune".into()),
        })
        .unwrap();
    assert_eq!(
        spans(&fixed),
        vec![(0, 200_000, PIECE)],
        "0.3 s at 1 Msps, exactly"
    );
    assert_eq!((fixed.samples, fixed.t0_ns, fixed.t1_ns), (PIECE, t0, t1));
    let data = std::fs::read(&fixed.data_path).unwrap();
    assert_eq!(data.len() as u64, 2 * PIECE);
    assert!(
        data == source_bytes(&recorded, 200_000, PIECE),
        "the fixed-tune clip is the source's IQ bytes"
    );
    eprintln!(
        "fixed-tune clip: {} samples from index 200000, identical to the source",
        fixed.samples
    );

    // --- Retune away and back ----------------------------------------------------------------------
    let out = handle.controller().retune(OTHER_HZ, FS).expect("retune");
    assert!(!out.replumbed, "same class and rate: tuned in place");
    wait_status("0.4 s at the new tuning", status, |s| {
        s.segments
            .iter()
            .any(|g| g.center_hz == OTHER_HZ && g.samples >= PIECE + 100_000)
    });
    let out = handle
        .controller()
        .retune(CENTER_HZ, FS)
        .expect("retune back");
    assert!(!out.replumbed);
    let s = wait_status("0.4 s back at the recording's tuning", status, |s| {
        s.segments
            .iter()
            .position(|g| g.center_hz == OTHER_HZ)
            .is_some_and(|k| {
                s.segments[k + 1..]
                    .iter()
                    .any(|g| g.center_hz == CENTER_HZ && g.samples >= PIECE + 100_000)
            })
    });
    let k = s
        .segments
        .iter()
        .position(|g| g.center_hz == OTHER_HZ)
        .unwrap();
    assert!(k > 0);
    let (before, away, back) = (
        s.segments[k - 1].clone(),
        s.segments[k].clone(),
        s.segments[k + 1].clone(),
    );
    assert_eq!(
        (before.center_hz, back.center_hz),
        (CENTER_HZ, CENTER_HZ),
        "{s:?}"
    );
    assert!(before.samples >= PIECE && away.samples >= PIECE, "{s:?}");
    let mut gaps = Vec::new();
    for (x, y) in [(&before, &away), (&away, &back)] {
        let gap = s
            .gaps
            .iter()
            .find(|gap| gap.before_segment == y.id)
            .expect("the settle skip after a retune is a gap");
        assert!(gap.samples > 0, "{gap:?}");
        assert_eq!(gap.samples, y.global_index - (x.global_index + x.samples));
        gaps.push(gap.samples);
    }
    assert_eq!(s.dropped_samples, 0, "lossless");
    assert_eq!(s.bytes, 2 * s.samples);
    assert!(s.disk_bytes >= s.bytes);

    // Clip 1 spans the retune away, clip 2 the retune back: exact index ranges.
    let before_end = before.global_index + before.samples;
    let away_end = away.global_index + away.samples;
    let clips = [
        (
            ClipRange::Index {
                start: before_end - PIECE,
                end: away.global_index + PIECE,
            },
            [(&before, before_end - PIECE), (&away, away.global_index)],
        ),
        (
            ClipRange::Index {
                start: away_end - PIECE,
                end: back.global_index + PIECE,
            },
            [(&away, away_end - PIECE), (&back, back.global_index)],
        ),
    ];
    let mut last = None;
    for (range, sides) in clips {
        let clip = export(range).unwrap();
        assert_eq!(
            spans(&clip),
            vec![(0, sides[0].1, PIECE), (PIECE, sides[1].1, PIECE)],
            "exact counts: {clip:?}"
        );
        assert_eq!(clip.samples, 2 * PIECE);
        let data = std::fs::read(&clip.data_path).unwrap();
        assert_eq!(data.len() as u64, 2 * clip.samples);
        for (p, (seg, g)) in clip.captures.iter().zip(sides) {
            assert_eq!((p.segment, p.center_hz), (seg.id, seg.center_hz));
            if seg.center_hz == CENTER_HZ {
                assert!(
                    piece(&data, p) == source_bytes(&recorded, g, PIECE),
                    "segment {} at the recording's tuning: the source's bytes",
                    seg.id
                );
            } else {
                let alone = export(ClipRange::Index {
                    start: g,
                    end: g + PIECE,
                })
                .unwrap();
                assert_eq!(spans(&alone), vec![(0, g, PIECE)]);
                assert!(
                    *piece(&data, p) == std::fs::read(&alone.data_path).unwrap()[..],
                    "segment {} in the retuned window: that stream range's bytes",
                    seg.id
                );
            }
        }
        // The ns form of the same range selects exactly the same samples.
        let by_ns = export(ClipRange::Time {
            t0_ns: clip.t0_ns,
            t1_ns: clip.t1_ns,
        })
        .unwrap();
        assert_eq!(spans(&by_ns), spans(&clip));
        let m = SigmfMeta::read(&clip.meta_path).unwrap();
        assert_eq!(m.global.sample_rate, Some(FS));
        assert_eq!(m.captures.len(), 2);
        for (c, info) in m.captures.iter().zip(&clip.captures) {
            assert_eq!(c.sample_start, info.sample_start);
            assert_eq!(c.frequency, Some(info.center_hz));
            assert_eq!(
                c.extra["core:global_index"].as_u64(),
                Some(info.global_index)
            );
            let p = c.provenance.as_ref().expect("per-capture provenance");
            assert_eq!(p.tune.center_hz, info.center_hz);
            assert_eq!((p.tune.lna_db, p.tune.vga_db), (info.lna_db, info.vga_db));
        }
        last = Some(clip);
    }
    let clip = last.unwrap();
    let repo = common::repo(&dir.0);
    let row = repo.recording(clip.id).unwrap();
    assert_eq!(row.kind, RecordingKind::IqSnippet);
    assert_eq!(row.size_bytes, 2 * clip.samples);
    assert_eq!(row.f_center_hz, OTHER_HZ);
    eprintln!(
        "retune clips: {PIECE} + {PIECE} samples each, away and back (centres {CENTER_HZ} / \
         {OTHER_HZ} Hz), settle gaps {gaps:?} samples"
    );

    // Refused before anything is written: a band outside every window, a clip over the cap,
    // malformed ranges.
    let files = || std::fs::read_dir(&recordings).unwrap().count();
    let n = files();
    let err = buffer
        .export_clip(&ClipRequest {
            range: ClipRange::Index {
                start: 0,
                end: u64::MAX,
            },
            band: Some((900e6, 901e6)),
            label: None,
        })
        .unwrap_err();
    assert!(matches!(err, ClipFailure::NotFound(_)), "{err:?}");
    let err = export(ClipRange::Index {
        start: 0,
        end: u64::MAX,
    })
    .unwrap_err();
    assert!(matches!(err, ClipFailure::TooLarge(_)), "{err:?}");
    for range in [
        ClipRange::Time {
            t0_ns: -8_900_000_000_000_000_000,
            t1_ns: 1,
        },
        ClipRange::Time { t0_ns: 5, t1_ns: 5 },
        ClipRange::Index { start: 5, end: 5 },
    ] {
        let err = export(range).unwrap_err();
        assert!(matches!(err, ClipFailure::Invalid(_)), "{range:?}: {err:?}");
    }
    assert_eq!(files(), n, "no file for a refused clip");
    let s = status();
    assert_eq!(
        (s.write_errors, s.failed_samples, s.pauses),
        (0, 0, 0),
        "{s:?}"
    );
    assert!(
        s.fs_free_bytes.is_some() && s.fs_total_bytes.is_some(),
        "{s:?}"
    );

    handle.stop();
    let (summary, fired) = wait_guarded(handle, LIMIT);
    assert!(!fired, "the run stopped");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
}
