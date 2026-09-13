//! Ring buffer under concurrency: dynamic reader attach/detach while the writer runs, with no
//! writer stall and exact sample accounting for every reader.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use hk_core::{
    BlockHeader, Discontinuity, ProvenanceHandle, ReadOutcome, RingConfig, TriggerWindow,
    ring_buffer,
};
use hk_model::{SampleTime, Timestamp};
use num_complex::Complex32;

const BLOCK: usize = 4096;

#[test]
fn readers_attach_and_detach_while_writer_runs_without_stall() {
    let (mut writer, ring) = ring_buffer::<Complex32>(RingConfig {
        sample_capacity: 1 << 18,
        block_capacity: 256,
    });
    let prov = ProvenanceHandle::new(common::provenance("synthetic:concurrency", 1e8, 2e6));
    let stop = Arc::new(AtomicBool::new(false));

    // Writer: paced at ~2 Msps, sample value encodes the stream index.
    let writer_stop = Arc::clone(&stop);
    let writer_thread = thread::spawn(move || {
        let mut samples = vec![Complex32::default(); BLOCK];
        let mut max_push = Duration::ZERO;
        let mut first = 0u64;
        let started = Instant::now();
        let period = Duration::from_secs_f64(BLOCK as f64 / 2e6);
        let mut blocks = 0u32;
        while !writer_stop.load(Ordering::Relaxed) {
            for (k, s) in samples.iter_mut().enumerate() {
                *s = Complex32::new((first + k as u64) as f32, 0.0);
            }
            let header = BlockHeader {
                time: SampleTime {
                    sample_index: first,
                    host_time: Timestamp::from_unix_nanos(first as i64 * 500),
                },
                provenance: prov.clone(),
                discontinuity: if first == 0 {
                    Discontinuity::STREAM_START
                } else {
                    Discontinuity::NONE
                },
                dropped_before: 0,
            };
            let t = Instant::now();
            writer.push(&header, &samples).unwrap();
            max_push = max_push.max(t.elapsed());
            first += BLOCK as u64;
            blocks += 1;
            if let Some(sleep) = (period * blocks).checked_sub(started.elapsed()) {
                thread::sleep(sleep);
            }
        }
        (max_push, first)
    });

    // 200 attach/detach cycles of short-lived readers across 4 threads, plus pre-trigger captures.
    let mut workers = Vec::new();
    for w in 0..4u64 {
        let ring = ring.clone();
        workers.push(thread::spawn(move || {
            let mut attach_worst = Duration::ZERO;
            for cycle in 0..50u64 {
                let t = Instant::now();
                let mut reader = ring.reader();
                attach_worst = attach_worst.max(t.elapsed());
                let mut buf = vec![Complex32::default(); 1000 + 97 * ((w + cycle) as usize % 7)];
                let mut expect = None;
                let mut got = 0u64;
                let deadline = Instant::now() + Duration::from_millis(3 + (cycle % 5));
                while Instant::now() < deadline {
                    match reader.read_timeout(&mut buf, Duration::from_millis(2)) {
                        ReadOutcome::Data(c) => {
                            if let Some(e) = expect {
                                assert_eq!(c.first_sample(), e, "contiguous");
                            }
                            for (k, s) in buf[..c.len].iter().enumerate() {
                                assert_eq!(s.re, (c.first_sample() + k as u64) as f32);
                            }
                            got += c.len as u64;
                            expect = Some(c.end_sample());
                        }
                        ReadOutcome::Overrun { resume_at, .. } => expect = Some(resume_at),
                        ReadOutcome::Empty => {}
                        ReadOutcome::Closed => break,
                    }
                }
                assert_eq!(reader.samples_read(), got);
                if cycle % 10 == 0 {
                    if let Some(next) = ring.next_sample().filter(|&n| n >= 20_000) {
                        let mut cap = ring
                            .pre_trigger(TriggerWindow {
                                trigger_sample: next,
                                pre_samples: 20_000,
                                post_samples: 8192,
                            })
                            .unwrap();
                        cap.wait(Duration::from_secs(2));
                        let got = cap.finish();
                        assert!(
                            got.is_gapless(),
                            "pre-trigger window gapless: {:?}",
                            got.segments
                        );
                        let start = next - 20_000;
                        for (k, s) in got.samples.iter().enumerate() {
                            assert_eq!(s.re, (start + k as u64) as f32);
                        }
                    }
                }
                drop(reader);
            }
            attach_worst
        }));
    }
    let attach_worst = workers
        .into_iter()
        .map(|h| h.join().unwrap())
        .max()
        .unwrap();
    stop.store(true, Ordering::Relaxed);
    let (max_push, written) = writer_thread.join().unwrap();
    assert!(written > 0);
    // A push of 4096 samples is microseconds; 20 ms would be a stall (generous for debug/CI).
    assert!(
        max_push < Duration::from_millis(20),
        "writer stalled: {max_push:?}"
    );
    assert!(
        attach_worst < Duration::from_millis(20),
        "attach took {attach_worst:?}"
    );
}

#[test]
fn lapped_reader_accounting_is_exact_under_a_running_writer() {
    let (mut writer, ring) = ring_buffer::<Complex32>(RingConfig {
        sample_capacity: 1 << 14,
        block_capacity: 64,
    });
    let prov = ProvenanceHandle::new(common::provenance("synthetic:lap", 1e8, 2e6));
    let mut reader = ring.reader();
    let writer_thread = thread::spawn(move || {
        let samples = vec![Complex32::new(1.0, 0.0); 1024];
        for b in 0..2000u64 {
            let header = BlockHeader {
                time: SampleTime {
                    sample_index: b * 1024,
                    host_time: Timestamp::UNIX_EPOCH,
                },
                provenance: prov.clone(),
                discontinuity: Discontinuity::NONE,
                dropped_before: 0,
            };
            writer.push(&header, &samples).unwrap();
        }
    });
    // A deliberately slow reader.
    let mut buf = vec![Complex32::default(); 512];
    let mut expect = 0u64;
    loop {
        match reader.read_timeout(&mut buf, Duration::from_millis(50)) {
            ReadOutcome::Data(c) => {
                assert_eq!(c.first_sample(), expect);
                assert!(
                    buf[..c.len].iter().all(|s| s.re == 1.0),
                    "never a torn sample"
                );
                expect = c.end_sample();
                thread::sleep(Duration::from_micros(50));
            }
            ReadOutcome::Overrun {
                lost_samples,
                gap_samples,
                resume_at,
            } => {
                assert_eq!(gap_samples, 0, "the writer made no gaps");
                assert_eq!(resume_at - lost_samples, expect);
                expect = resume_at;
            }
            ReadOutcome::Closed => break,
            ReadOutcome::Empty => {}
        }
    }
    writer_thread.join().unwrap();
    assert_eq!(expect, 2000 * 1024);
    assert_eq!(reader.samples_read() + reader.lost_samples(), 2000 * 1024);
    assert!(
        reader.overruns() > 0,
        "the slow reader should have been lapped"
    );
}
