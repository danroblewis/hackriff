//! T3 replay tests: the SigMF replay source through the `Source` API, and into the ring.
//! Acceptance (T-003): replay a SigMF file deterministically with provenance attached.

mod common;

use std::io::Cursor;
use std::time::{Duration, Instant};

use common::*;
use hk_core::source::format;
use hk_core::{
    Discontinuity, Pacing, ReadOutcome, ReplayOptions, RingConfig, SigmfReplaySource, Source,
    SourceError, ring_buffer,
};
use hk_model::sigmf::{Datatype, SigmfMeta};
use hk_model::{Timestamp, TimestampMethod, sigmf::PROVENANCE_KEY};
use num_complex::{Complex, Complex32};

fn opts(block_len: usize) -> ReplayOptions {
    ReplayOptions {
        block_len,
        pacing: Pacing::Unpaced,
    }
}

#[test]
fn deterministic_replay_checksum_is_identical_across_runs() {
    let dir = TempDir::new("determinism");
    let mut rng = Rng::new(7);
    let data: Vec<u8> = (0..20_000 * 2).map(|_| rng.next_u64() as u8).collect();
    let mut meta = meta(Datatype::Ci8, 250_000.0);
    meta.global.provenance = Some(provenance("synthetic:test", 433.92e6, 250_000.0));
    meta.captures.push(capture(0, 433.92e6));
    meta.captures[0].datetime = Some("2026-09-13T12:00:00Z".into());
    let path = write_recording(dir.path(), "random", &meta, &data);

    let run = || {
        let mut src = SigmfReplaySource::open(&path, opts(4096)).unwrap();
        drain(&mut src)
    };
    let (h1, sum1) = run();
    let (h2, sum2) = run();
    assert_eq!(sum1, sum2, "replay must be bit-identical across runs");
    assert_eq!(h1.len(), h2.len());

    // Independent expectation: checksum of the raw bytes decoded directly.
    let mut expected = Fnv::new();
    let mut decoded = Vec::new();
    format::decode_into(Datatype::Ci8, &data, &mut decoded).unwrap();
    let t0 = hk_core::source::sigmf_replay::parse_sigmf_datetime("2026-09-13T12:00:00Z").unwrap();
    for (i, chunk) in decoded.chunks(4096).enumerate() {
        let first = (i * 4096) as u64;
        expected.u64(first);
        expected.u64((t0.as_unix_nanos() + first as i64 * 4000) as u64); // 1/250 kHz = 4000 ns
        let flags = if i == 0 {
            Discontinuity::STREAM_START
        } else {
            Discontinuity::NONE
        };
        expected.u64(u64::from(flags.bits()));
        expected.u64(0);
        expected.samples(chunk);
    }
    assert_eq!(sum1, expected.finish());

    // A different block length gives the same samples in the same order.
    let mut src = SigmfReplaySource::open(&path, opts(1000)).unwrap();
    let mut all = Vec::new();
    let mut buf = Vec::new();
    while src.read_block(&mut buf).unwrap().is_some() {
        all.extend_from_slice(&buf);
    }
    assert_eq!(all, decoded);
}

#[test]
fn provenance_is_attached_to_every_block() {
    let dir = TempDir::new("provenance");
    let mut meta = meta(Datatype::Ci8, 1e6);
    let prov = provenance("hackrf:0000000000000000a06063c8234e925f", 1090e6, 1e6);
    meta.global.provenance = Some(prov.clone());
    meta.captures.push(capture(0, 1090e6));
    let path = write_recording(dir.path(), "prov", &meta, &ramp_ci8(10_000));

    let mut src = SigmfReplaySource::open(&path, opts(777)).unwrap();
    assert_eq!(src.total_samples(), Some(10_000));
    let (headers, _) = drain(&mut src);
    assert_eq!(headers.len(), 10_000usize.div_ceil(777));
    let first_id = headers[0].0.provenance.id();
    let mut expect = 0;
    for (h, len) in &headers {
        assert_eq!(h.first_sample(), expect, "monotonic, contiguous counter");
        assert_eq!(
            *h.provenance.get(),
            prov,
            "hackriff:provenance carried verbatim"
        );
        assert_eq!(h.provenance.id(), first_id, "one deduplicated record");
        expect += *len as u64;
    }
    assert_eq!(expect, 10_000);

    // Third-party file without hackriff:provenance: synthesised, marked unknown.
    let mut foreign = meta.clone();
    foreign.global.provenance = None;
    foreign.global.hw = Some("rtl-sdr v3".into());
    let mut src =
        SigmfReplaySource::from_reader(foreign, Cursor::new(ramp_ci8(100)), opts(64)).unwrap();
    let block = src.next_block().unwrap().unwrap();
    let p = block.header.provenance.get();
    assert_eq!(p.timestamp_method, TimestampMethod::Unknown);
    assert_eq!(p.device_id, "sigmf:rtl-sdr v3");
    assert_eq!(p.tune.center_hz, 1090e6);
    assert_eq!(p.tune.sample_rate_hz, 1e6);
    let block = src.next_block().unwrap().unwrap();
    assert_eq!(
        block.header.provenance.get().timestamp_method,
        TimestampMethod::Unknown
    );
    assert!(src.next_block().unwrap().is_none());
}

#[test]
fn multi_segment_recording_flags_retune_and_gap() {
    let fs = 10_000.0;
    let mut meta = meta(Datatype::Ci8, fs);
    meta.global.provenance = Some(provenance("synthetic:test", 100e6, fs));
    meta.captures.push(capture(0, 100e6));
    meta.captures[0].datetime = Some("2026-09-13T00:00:00Z".into());
    // Capture 1: same frequency, but the original stream skipped 500 samples.
    let mut c1 = capture(1000, 100e6);
    c1.extra
        .insert("core:global_index".into(), serde_json::json!(1500));
    meta.captures.push(c1);
    // Capture 2: retune to 200 MHz with its own provenance (different gain too).
    let mut c2 = capture(2000, 200e6);
    let mut p2 = provenance("synthetic:test", 200e6, fs);
    p2.tune.lna_db = 32.0;
    c2.provenance = Some(p2.clone());
    meta.captures.push(c2);

    let data = ramp_ci8(3000);
    let mut src = SigmfReplaySource::from_reader(meta, Cursor::new(data), opts(256)).unwrap();
    let (headers, _) = drain(&mut src);

    let lens: Vec<usize> = headers.iter().map(|(_, n)| *n).collect();
    assert_eq!(
        &lens[..4],
        &[256, 256, 256, 232],
        "blocks never span a capture boundary"
    );

    let at = |counter: u64| {
        headers
            .iter()
            .find(|(h, _)| h.first_sample() == counter)
            .unwrap_or_else(|| panic!("no block at {counter}"))
            .0
            .clone()
    };
    let b0 = at(0);
    assert_eq!(b0.discontinuity, Discontinuity::STREAM_START);

    let b1 = at(1500);
    assert!(b1.discontinuity.contains(Discontinuity::GAP));
    assert!(!b1.discontinuity.contains(Discontinuity::RETUNE));
    assert_eq!(b1.dropped_before, 500);
    assert_eq!(
        b1.provenance, b0.provenance,
        "unchanged state shares the record"
    );
    // Time extrapolated across the gap from the first capture's datetime: 1500 / 10 kHz = 150 ms.
    assert_eq!(
        b1.time.host_time.as_unix_nanos() - b0.time.host_time.as_unix_nanos(),
        150_000_000
    );

    let b2 = at(2500);
    assert!(b2.discontinuity.contains(Discontinuity::RETUNE));
    assert!(b2.discontinuity.contains(Discontinuity::GAIN_CHANGE));
    assert!(b2.discontinuity.contains(Discontinuity::PROVENANCE_CHANGE));
    assert!(!b2.discontinuity.contains(Discontinuity::GAP));
    assert_eq!(*b2.provenance.get(), p2);
    assert_eq!(b2.center_hz(), 200e6);

    // Mid-segment blocks carry no flags, and the counter is monotonic across all blocks.
    let mut end = 0;
    for (h, n) in &headers {
        assert!(h.first_sample() >= end);
        if h.first_sample() > end {
            assert!(h.discontinuity.contains(Discontinuity::GAP));
            assert_eq!(h.dropped_before, h.first_sample() - end);
        }
        end = h.first_sample() + *n as u64;
    }
    assert_eq!(end, 3500);

    // Through the ring: the reader sees the same flags and gap.
    let mut src = SigmfReplaySource::from_reader(
        {
            let mut m = common::meta(Datatype::Ci8, fs);
            m.captures.push(capture(0, 100e6));
            let mut c = capture(1000, 300e6);
            c.extra
                .insert("core:global_index".into(), serde_json::json!(1200));
            m.captures.push(c);
            m
        },
        Cursor::new(ramp_ci8(2000)),
        opts(300),
    )
    .unwrap();
    let (mut w, ring) = ring_buffer::<Complex32>(RingConfig {
        sample_capacity: 4096,
        block_capacity: 64,
    });
    let mut reader = ring.reader();
    let mut buf = Vec::with_capacity(300);
    while let Some(h) = src.read_block(&mut buf).unwrap() {
        w.push(&h, &buf).unwrap();
    }
    let mut out = vec![Complex32::default(); 4096];
    let mut flagged = Vec::new();
    while let ReadOutcome::Data(c) = reader.read(&mut out) {
        if !c.discontinuity.is_empty() {
            flagged.push((c.first_sample(), c.discontinuity, c.dropped_before));
        }
    }
    assert_eq!(flagged.len(), 2);
    assert_eq!(flagged[0], (0, Discontinuity::STREAM_START, 0));
    assert_eq!(flagged[1].0, 1200);
    assert!(
        flagged[1]
            .1
            .contains(Discontinuity::RETUNE | Discontinuity::GAP)
    );
    assert_eq!(flagged[1].2, 200);
}

#[test]
fn all_supported_datatypes_replay() {
    let values = [(-1.0f32, 0.5f32), (0.25, -0.75)];
    let cases: [(Datatype, Vec<u8>); 4] = [
        (Datatype::Ci8, vec![0x80, 0x40, 0x20, 0xa0]),
        (Datatype::Cu8, vec![0x00, 0xc0, 0xa0, 0x20]),
        (Datatype::Ci16Le, {
            let mut b = Vec::new();
            for v in [-32768i16, 16384, 8192, -24576] {
                b.extend_from_slice(&v.to_le_bytes());
            }
            b
        }),
        (Datatype::Cf32Le, {
            let mut b = Vec::new();
            for (re, im) in values {
                b.extend_from_slice(&re.to_le_bytes());
                b.extend_from_slice(&im.to_le_bytes());
            }
            b
        }),
    ];
    for (dt, bytes) in cases {
        let mut src =
            SigmfReplaySource::from_reader(meta(dt, 1000.0), Cursor::new(bytes), opts(16)).unwrap();
        let block = src.next_block().unwrap().unwrap();
        let expect: Vec<_> = values
            .iter()
            .map(|&(re, im)| Complex32::new(re, im))
            .collect();
        assert_eq!(block.samples, expect, "{dt}");
        assert_eq!(src.capabilities().native_format, dt);
    }
    let err =
        SigmfReplaySource::from_reader(meta(Datatype::Rf32Le, 1.0), Cursor::new(vec![]), opts(1));
    assert!(matches!(err, Err(SourceError::UnsupportedDatatype(_))));
}

#[test]
fn native_ci8_path_matches_normalised_path() {
    let data = ramp_ci8(1000);
    let mut a = SigmfReplaySource::from_reader(
        meta(Datatype::Ci8, 1e3),
        Cursor::new(data.clone()),
        opts(128),
    )
    .unwrap();
    let mut b =
        SigmfReplaySource::from_reader(meta(Datatype::Ci8, 1e3), Cursor::new(data), opts(128))
            .unwrap();
    let (mut fa, mut fb) = (Vec::new(), Vec::new());
    loop {
        let ha = a.read_block(&mut fa).unwrap();
        let hb = b.read_block_ci8(&mut fb).unwrap();
        match (ha, hb) {
            (None, None) => break,
            (Some(ha), Some(hb)) => {
                assert_eq!(ha.time, hb.time);
                let norm: Vec<Complex32> = fb
                    .iter()
                    .map(|s: &Complex<i8>| {
                        Complex32::new(f32::from(s.re) / 128.0, f32::from(s.im) / 128.0)
                    })
                    .collect();
                assert_eq!(norm, fa);
            }
            other => panic!("paths diverged: {other:?}"),
        }
    }
    let mut c = SigmfReplaySource::from_reader(
        meta(Datatype::Cf32Le, 1e3),
        Cursor::new(vec![0; 8]),
        opts(4),
    )
    .unwrap();
    assert!(matches!(
        c.read_block_ci8(&mut fb),
        Err(SourceError::Unsupported { .. })
    ));
}

#[test]
fn replay_is_not_controllable_and_stops() {
    let mut src = SigmfReplaySource::from_reader(
        meta(Datatype::Ci8, 1e3),
        Cursor::new(ramp_ci8(100)),
        opts(10),
    )
    .unwrap();
    assert!(!src.capabilities().controllable);
    assert!(!src.capabilities().tx_capable);
    let control = src.control();
    assert!(matches!(
        control.tune(1e6),
        Err(SourceError::Unsupported { .. })
    ));
    assert!(matches!(
        control.set_bias_tee(true),
        Err(SourceError::Unsupported { .. })
    ));
    assert!(src.next_block().unwrap().is_some());
    // The control handle stops the stream from another thread, without touching the stream.
    std::thread::spawn(move || control.stop().unwrap())
        .join()
        .unwrap();
    assert!(src.next_block().unwrap().is_none());
}

#[test]
fn real_time_pacing_delays_but_does_not_change_content() {
    let data = ramp_ci8(2000);
    let fs = 10_000.0; // 2000 samples = 200 ms at 1×; 100 ms at 2×.
    let mut paced = SigmfReplaySource::from_reader(
        meta(Datatype::Ci8, fs),
        Cursor::new(data.clone()),
        ReplayOptions {
            block_len: 200,
            pacing: Pacing::RealTime { speed: 2.0 },
        },
    )
    .unwrap();
    let started = Instant::now();
    let (_, paced_sum) = drain(&mut paced);
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(95),
        "paced replay took {elapsed:?}"
    );
    let mut unpaced =
        SigmfReplaySource::from_reader(meta(Datatype::Ci8, fs), Cursor::new(data), opts(200))
            .unwrap();
    assert_eq!(drain(&mut unpaced).1, paced_sum);
}

#[test]
fn malformed_recordings_are_explicit_errors() {
    // Truncated mid-sample.
    let mut src = SigmfReplaySource::from_reader(
        meta(Datatype::Ci16Le, 1e3),
        Cursor::new(vec![0; 7]),
        opts(8),
    )
    .unwrap();
    assert!(matches!(
        src.next_block(),
        Err(SourceError::InvalidRecording(_))
    ));
    // Missing sample rate.
    let no_rate = SigmfMeta::new(Datatype::Ci8);
    assert!(matches!(
        SigmfReplaySource::from_reader(no_rate, Cursor::new(vec![]), opts(8)),
        Err(SourceError::InvalidRecording(_))
    ));
    // global_index going backwards.
    let mut m = meta(Datatype::Ci8, 1e3);
    m.captures.push(capture(0, 1e6));
    let mut c = capture(10, 1e6);
    c.extra
        .insert("core:global_index".into(), serde_json::json!(5));
    m.captures.push(c);
    assert!(matches!(
        SigmfReplaySource::from_reader(m, Cursor::new(vec![]), opts(8)),
        Err(SourceError::InvalidRecording(_))
    ));
}

#[test]
fn tiny_tone_fixture_replays_with_its_provenance() {
    let meta_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/tiny/tone.sigmf-meta"
    );
    let data_path = hk_model::sigmf::data_path_for(meta_path);
    let len = std::fs::metadata(&data_path).map(|m| m.len()).unwrap_or(0);
    if len != 8192 {
        eprintln!(
            "skipping: {} is not materialised (git lfs pull)",
            data_path.display()
        );
        return;
    }
    let mut src = SigmfReplaySource::open(meta_path, opts(1024)).unwrap();
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(meta_path).unwrap()).unwrap();
    assert_eq!(raw["global"][PROVENANCE_KEY]["device_id"], "synthetic:hkpy");
    let (headers, sum) = drain(&mut src);
    assert_eq!(headers.len(), 4);
    for (h, _) in &headers {
        assert_eq!(h.provenance.device_id, "synthetic:hkpy");
        assert_eq!(h.provenance.timestamp_method, TimestampMethod::Synthetic);
        assert_eq!(h.center_hz(), 100e6);
    }
    let mut again = SigmfReplaySource::open(meta_path, opts(1024)).unwrap();
    assert_eq!(drain(&mut again).1, sum);
    // Amplitude 100 counts: |z| = 100/128 within quantisation.
    let block = SigmfReplaySource::open(meta_path, opts(4096))
        .unwrap()
        .next_block()
        .unwrap()
        .unwrap();
    for s in &block.samples {
        assert!((s.norm() - 100.0 / 128.0).abs() < 0.01);
    }
}

#[test]
fn header_and_trailing_bytes_are_skipped() {
    let dir = TempDir::new("ncd");
    let mut meta = meta(Datatype::Ci8, 1e3);
    let mut c0 = capture(0, 1e6);
    c0.extra
        .insert("core:header_bytes".into(), serde_json::json!(6));
    let mut c1 = capture(100, 1e6);
    c1.extra
        .insert("core:header_bytes".into(), serde_json::json!(4));
    meta.captures = vec![c0, c1];
    meta.global
        .extra
        .insert("core:trailing_bytes".into(), serde_json::json!(3));
    let ramp = ramp_ci8(150);
    let mut data = vec![0xaa; 6];
    data.extend_from_slice(&ramp[..200]);
    data.extend_from_slice(&[0xbb; 4]);
    data.extend_from_slice(&ramp[200..]);
    data.extend_from_slice(&[0xcc; 3]);
    let path = write_recording(dir.path(), "ncd", &meta, &data);

    let mut src = SigmfReplaySource::open(&path, opts(64)).unwrap();
    assert_eq!(src.total_samples(), Some(150));
    let mut all = Vec::new();
    let mut buf = Vec::new();
    while src.read_block(&mut buf).unwrap().is_some() {
        all.extend_from_slice(&buf);
    }
    let mut expected = Vec::new();
    format::decode_into(Datatype::Ci8, &ramp, &mut expected).unwrap();
    assert_eq!(
        all, expected,
        "header and trailing bytes never become samples"
    );

    // A reader of unknown length cannot tell where the trailing bytes start.
    assert!(matches!(
        SigmfReplaySource::from_reader(meta, Cursor::new(data), opts(64)),
        Err(SourceError::InvalidRecording(_))
    ));
}

#[test]
fn recordings_without_datetime_are_marked_untimed() {
    let mut m = meta(Datatype::Ci8, 1e3);
    let mut p = provenance("hackrf:test", 1e6, 1e3);
    p.timestamp_method = TimestampMethod::HostArrival;
    p.timestamp_error_budget_ns = Some(1000);
    m.global.provenance = Some(p);
    m.captures.push(capture(0, 1e6));
    let mut c1 = capture(50, 1e6);
    c1.datetime = Some("2026-09-13T00:00:00Z".into());
    m.captures.push(c1);
    let mut src = SigmfReplaySource::from_reader(m, Cursor::new(ramp_ci8(100)), opts(50)).unwrap();

    // No anchor yet: epoch-relative times, and the provenance says the method is unknown.
    let b0 = src.next_block().unwrap().unwrap();
    assert_eq!(b0.header.time.host_time, Timestamp::UNIX_EPOCH);
    assert_eq!(
        b0.header.provenance.timestamp_method,
        TimestampMethod::Unknown
    );
    assert_eq!(b0.header.provenance.timestamp_error_budget_ns, None);

    // Anchored by core:datetime: the recorded method applies again.
    let b1 = src.next_block().unwrap().unwrap();
    assert_eq!(
        b1.header.provenance.timestamp_method,
        TimestampMethod::HostArrival
    );
    assert!(
        b1.header
            .discontinuity
            .contains(Discontinuity::PROVENANCE_CHANGE)
    );
}

#[test]
fn sample_counter_overflow_is_an_error() {
    let mut m = meta(Datatype::Ci8, 1e3);
    let mut c = capture(0, 1e6);
    c.extra
        .insert("core:global_index".into(), serde_json::json!(u64::MAX - 5));
    m.captures.push(c);
    let mut src =
        SigmfReplaySource::from_reader(m.clone(), Cursor::new(ramp_ci8(10)), opts(16)).unwrap();
    assert!(matches!(
        src.next_block(),
        Err(SourceError::InvalidRecording(_))
    ));

    let dir = TempDir::new("overflow");
    let path = write_recording(dir.path(), "overflow", &m, &ramp_ci8(10));
    assert!(matches!(
        SigmfReplaySource::open(&path, opts(16)),
        Err(SourceError::InvalidRecording(_))
    ));
}
