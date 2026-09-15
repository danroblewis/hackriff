//! T-157: the rolling IQ capture buffer end to end **through the mock SDR device interface** (a
//! tone recording behind `MockSdrDriver`, retuned through the pipeline controller), never feeding
//! files into the pipeline.
//!
//! - A clip at the recording's own tuning is the recording's bytes, exactly (the mock passes its
//!   own centre and rate through bit-exact), with stream indices and times on the sample clock.
//! - A clip spanning a retune has one SigMF capture per side, each with its own centre and
//!   provenance, sample starts that add up, and the settle gap explicit in `core:global_index`;
//!   the status lists both segments and the gap; a `Recording` row is stored.

mod common;

use std::time::{Duration, Instant};

use common::{TempDir, tone_recording, wait_guarded};
use hk_core::{MockEnd, MockOptions, MockSdrDriver, Source};
use hk_model::RecordingKind;
use hk_model::sigmf::SigmfMeta;
use hk_pipeline::class::window_class;
use hk_pipeline::iqbuffer::{ClipFailure, ClipRequest};
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, replay_plan};
use hk_store::iqbuffer::{IqBufferConfig, IqBufferStatus};

const FS: f64 = 1.0e6;
const CENTER_HZ: f64 = 100.0e6;
const OTHER_HZ: f64 = 100.2e6;
const LIMIT: Duration = Duration::from_secs(120);

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
        max_bytes: 1 << 30,
        max_s: 600.0,
    };
    let t_start = info.start_time.as_unix_nanos() as f64 / 1e9;
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

    // --- Fixed tune: the clip is the recording's bytes -------------------------------------------
    let s = wait_status("1 s buffered", status, |s| s.samples >= FS as u64);
    let seg0 = s.segments[0].clone();
    assert_eq!(seg0.global_index, 0, "buffered from the stream start");
    assert_eq!(seg0.center_hz, CENTER_HZ);
    assert_eq!(seg0.sample_rate_hz, FS);
    assert!(seg0.samples >= FS as u64, "{s:?}");
    assert!(
        (seg0.t0 - t_start).abs() < 1e-9,
        "sample clock: {} vs {t_start}",
        seg0.t0
    );
    let fixed = buffer
        .export_clip(&ClipRequest {
            t0_s: seg0.t0 + 0.2,
            t1_s: seg0.t0 + 0.5,
            band: None,
            label: Some("fixed tune".into()),
        })
        .unwrap();
    assert_eq!(fixed.captures.len(), 1, "{fixed:?}");
    let n = fixed.samples;
    assert!(n.abs_diff(300_000) <= 1, "0.3 s at 1 Msps: {n}");
    let g = fixed.captures[0].global_index;
    assert!(g.abs_diff(200_000) <= 1, "{g}");
    let data = std::fs::read(&fixed.data_path).unwrap();
    assert_eq!(data.len() as u64, 2 * n);
    assert!(
        data == recorded[(2 * g) as usize..(2 * (g + n)) as usize],
        "the fixed-tune clip is the source's IQ bytes"
    );
    eprintln!(
        "fixed-tune clip: {n} samples from index {g}, {} bytes identical to the source",
        data.len()
    );

    // --- Retune: the clip spans both windows ------------------------------------------------------
    let out = handle.controller().retune(OTHER_HZ, FS).expect("retune");
    assert!(!out.replumbed, "same class and rate: tuned in place");
    let s = wait_status("0.5 s at the new tuning", status, |s| {
        s.segments
            .iter()
            .any(|g| g.center_hz == OTHER_HZ && g.samples >= FS as u64 / 2)
    });
    let k = s
        .segments
        .iter()
        .position(|g| g.center_hz == OTHER_HZ)
        .unwrap();
    assert!(k > 0);
    let (before, after) = (s.segments[k - 1].clone(), s.segments[k].clone());
    assert_eq!(before.center_hz, CENTER_HZ, "{s:?}");
    assert!(before.samples >= 300_000, "{s:?}");
    let gap = s
        .gaps
        .iter()
        .find(|gap| gap.before_segment == after.id)
        .expect("the settle skip after the retune is a gap");
    assert!(gap.samples > 0, "{gap:?}");
    assert_eq!(
        gap.samples,
        after.global_index - (before.global_index + before.samples)
    );
    assert_eq!(s.dropped_samples, 0, "lossless");
    assert_eq!(s.bytes, 2 * s.samples);
    assert!(s.disk_bytes >= s.bytes);

    let clip = buffer
        .export_clip(&ClipRequest {
            t0_s: before.t1 - 0.3,
            t1_s: after.t0 + 0.3,
            band: None,
            label: None,
        })
        .unwrap();
    assert_eq!(clip.captures.len(), 2, "{clip:?}");
    let (a, b) = (&clip.captures[0], &clip.captures[1]);
    assert_eq!((a.center_hz, b.center_hz), (CENTER_HZ, OTHER_HZ));
    assert_eq!((a.segment, b.segment), (before.id, after.id));
    assert_eq!(a.sample_start, 0);
    assert!(a.samples.abs_diff(300_000) <= 1, "{a:?}");
    assert!(b.samples.abs_diff(300_000) <= 1, "{b:?}");
    assert_eq!(b.sample_start, a.samples);
    assert_eq!(clip.samples, a.samples + b.samples);
    assert_eq!(
        a.global_index + a.samples,
        before.global_index + before.samples
    );
    assert_eq!(b.global_index, after.global_index);
    assert_eq!(
        std::fs::metadata(&clip.data_path).unwrap().len(),
        2 * clip.samples
    );
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
    let repo = common::repo(&dir.0);
    let row = repo.recording(clip.id).unwrap();
    assert_eq!(row.kind, RecordingKind::IqSnippet);
    assert_eq!(row.size_bytes, 2 * clip.samples);
    assert_eq!(row.f_center_hz, CENTER_HZ);
    eprintln!(
        "retune clip: {} + {} samples (centres {} / {} Hz), settle gap {} samples",
        a.samples, b.samples, a.center_hz, b.center_hz, gap.samples
    );

    // A band outside both windows exports nothing.
    let err = buffer
        .export_clip(&ClipRequest {
            t0_s: before.t1 - 0.3,
            t1_s: after.t0 + 0.3,
            band: Some((900e6, 901e6)),
            label: None,
        })
        .unwrap_err();
    assert!(matches!(err, ClipFailure::NotFound(_)), "{err:?}");

    handle.stop();
    let (summary, fired) = wait_guarded(handle, LIMIT);
    assert!(!fired, "the run stopped");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
}
