//! T-857 (MAUTO M-6; ADR-0015 §5.3, §6, §14): a region analysis's acquisition end to end
//! **through the mock SDR device interface** — a tone recording behind `MockSdrDriver` feeds the
//! run's IQ ring, never a file handed to the pipeline.
//!
//! - **The ring read** returns each burst's guarded window in memory as segment chunks with
//!   provenance, byte-identical to the source recording at the stream indices the sample clock
//!   names.
//! - **The burst set** keeps the target's bursts only (an off-band burst is not read), guards and
//!   separates them, marks the odd ones hold-out, and concatenates them with a `DISCONTINUITY`
//!   (`STREAM_START`, then `GAP`) — never spliced.
//! - **Coverage is what was read**: a burst before the ring's first sample is `missing`, adds no
//!   coverage, and the window's samples are exactly the chunks'.
//! - **Pin on analyze**: the chunks are stored as one pinned `iq-snippet` Recording with trigger
//!   `analyze`, whose data file is byte-identical to what the search reads, one SigMF capture
//!   per chunk with its own `core:global_index`.

mod common;

use std::time::{Duration, Instant};

use common::{TempDir, tone_recording, wait_guarded};
use hk_core::{Discontinuity, MockEnd, MockOptions, MockSdrDriver, Source};
use hk_model::sigmf::SigmfMeta;
use hk_model::{
    FreqRange, RecordingKind, RecordingTrigger, Repository, RetentionClass, TimeRange, Timestamp,
};
use hk_pipeline::class::window_class;
use hk_pipeline::synth::acquire::{
    BurstSet, BurstSighting, BurstTarget, Membership, Missing, acquire_and_pin,
};
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, replay_plan};
use hk_store::iqbuffer::IqBufferConfig;

const FS: f64 = 1.0e6;
const CENTER_HZ: f64 = 915.0e6;
const LIMIT: Duration = Duration::from_secs(120);
const MS: i64 = 1_000_000;

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

fn burst(t0: i64, start_ms: i64, len_ms: i64, f_hz: f64) -> BurstSighting {
    BurstSighting {
        time: TimeRange::new(
            Timestamp::from_unix_nanos(t0 + start_ms * MS),
            Timestamp::from_unix_nanos(t0 + (start_ms + len_ms) * MS),
        ),
        freq: FreqRange::centered(f_hz, 40e3),
        emitter: None,
        cluster_id: None,
    }
}

#[test]
fn analyze_acquires_a_burst_set_from_the_ring_and_pins_exactly_what_it_read() {
    let dir = TempDir::new("t857-acquire");
    let meta = tone_recording(&dir.0.join("rec"), "tone", FS, 20.0, CENTER_HZ, None);
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
    cfg.settings.chains = Some(Vec::new());
    cfg.iq_buffer = IqBufferConfig {
        enabled: Some(true),
        retention_s: 600.0,
        max_bytes: Some(256 << 20),
        min_free_bytes: Some(0),
        max_clip_bytes: 16 << 20,
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
    let buffer = handle.iq_buffer();
    assert!(buffer.wait_allocated(LIMIT));
    assert!(buffer.enabled());
    let deadline = Instant::now() + LIMIT;
    let s = loop {
        let s = buffer.status(None, None, 100);
        if s.samples >= 2 * FS as u64 {
            break s;
        }
        assert!(Instant::now() < deadline, "timed out buffering 2 s: {s:?}");
        std::thread::sleep(Duration::from_millis(20));
    };
    let seg0 = s.segments[0].clone();
    assert_eq!(seg0.global_index, 0);
    let t0 = seg0.t0_ns;
    assert_eq!(t0, info.start_time.as_unix_nanos(), "sample clock");

    // Three 5 ms bursts of the target, one off band, one before the ring's first sample.
    let sightings = [
        burst(t0, 300, 5, CENTER_HZ + 50e3),
        burst(t0, 600, 5, CENTER_HZ + 50e3),
        burst(t0, 900, 5, CENTER_HZ + 50e3),
        burst(t0, 750, 5, CENTER_HZ + 400e3),
        burst(t0, -5_000, 5, CENTER_HZ + 50e3),
    ];
    let target = BurstTarget {
        emitter: None,
        cluster_id: None,
        band: FreqRange::centered(CENTER_HZ + 50e3, 60e3),
    };
    let window = TimeRange::new(
        Timestamp::from_unix_nanos(t0 - 10_000 * MS),
        Timestamp::from_unix_nanos(t0 + 1_500 * MS),
    );
    let set = BurstSet::collect(&target, &sightings, window);
    assert_eq!(
        set.bursts.len(),
        4,
        "the off-band burst is not the target's"
    );
    assert!(set.bursts.iter().all(|b| b.membership == Membership::Band));
    let holdout: Vec<bool> = set.bursts.iter().map(|b| b.holdout).collect();
    assert_eq!(holdout, vec![false, true, false, true]);

    let band = Some((target.band.lo_hz, target.band.hi_hz));
    let acq = acquire_and_pin(&buffer, &set, band, Some("t857 analyze")).unwrap();

    // The burst before the ring's first sample contributed nothing, and says so.
    assert_eq!(acq.missing, vec![(0, Missing::NotBuffered)]);
    // Each buffered burst is one chunk: its guarded window (±5 ms), exact on the sample clock.
    assert_eq!(acq.chunks.len(), 3, "{:?}", acq.chunks.len());
    let want_starts = [295u64, 595, 895];
    for (c, (&ms, burst)) in acq.chunks.iter().zip(want_starts.iter().zip(1usize..)) {
        let g = ms * 1_000;
        assert_eq!(c.burst, burst);
        assert_eq!(c.holdout, burst % 2 == 1);
        assert_eq!(c.chunk.piece.global_index, g);
        assert_eq!(c.chunk.piece.samples, 15_000);
        assert_eq!(c.chunk.piece.t_ns, t0 + ms as i64 * MS);
        assert_eq!(c.chunk.piece.provenance.tune.center_hz, CENTER_HZ);
        assert!(
            c.chunk.data == source_bytes(&recorded, g, 15_000),
            "burst {burst}'s chunk is the source's IQ"
        );
    }
    // Concatenated with a DISCONTINUITY between bursts, never spliced.
    assert_eq!(acq.chunks[0].discontinuity, Discontinuity::STREAM_START);
    assert_eq!(acq.chunks[1].discontinuity, Discontinuity::GAP);
    assert_eq!(acq.chunks[2].discontinuity, Discontinuity::GAP);
    assert_eq!(acq.split(true).count(), 2, "bursts 1 and 3 are hold-out");
    assert_eq!(acq.split(false).count(), 1);

    // Coverage is exactly what was read.
    let w = acq.window();
    assert_eq!(w.samples, 45_000);
    assert_eq!(acq.ledger.covered_ns(), 45 * MS);
    assert_eq!((w.bursts, w.bursts_missing, w.gaps), (3, 1, 2));
    assert_eq!(w.skipped_ns, ((595 - 310) + (895 - 610)) * MS);
    assert_eq!(w.t_lo, Some(Timestamp::from_unix_nanos(t0 + 295 * MS)));
    assert_eq!(w.t_hi, Some(Timestamp::from_unix_nanos(t0 + 910 * MS)));

    // Pinned before any search: a Recording with trigger `analyze`, retention pinned, whose data
    // is byte-identical to the chunks and whose captures keep each burst's own stream index.
    let pinned = acq.pinned.as_ref().expect("pinned");
    assert_eq!(w.clip_id, Some(pinned.id));
    let data = std::fs::read(&pinned.data_path).unwrap();
    let read: Vec<u8> = acq
        .chunks
        .iter()
        .flat_map(|c| c.chunk.data.clone())
        .collect();
    assert!(data == read, "the pinned clip is exactly what was read");
    let captures: Vec<(u64, u64)> = pinned
        .captures
        .iter()
        .map(|c| (c.sample_start, c.global_index))
        .collect();
    assert_eq!(
        captures,
        vec![(0, 295_000), (15_000, 595_000), (30_000, 895_000)]
    );
    let meta = SigmfMeta::read(std::path::Path::new(&pinned.meta_path)).unwrap();
    assert_eq!(meta.captures.len(), 3);
    let repo = Repository::open(dir.0.join("hackriff.db")).unwrap();
    let row = repo.recording(pinned.id).unwrap();
    assert_eq!(row.trigger, RecordingTrigger::Analyze);
    assert_eq!(row.retention_class, RetentionClass::Pinned);
    assert_eq!(row.kind, RecordingKind::IqSnippet);
    assert_eq!(row.size_bytes, 2 * 45_000);

    handle.stop();
    let (_, fired) = wait_guarded(handle, LIMIT);
    assert!(!fired, "the run stopped cleanly");
}
