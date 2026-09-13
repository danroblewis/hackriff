//! Burst tracking (C10, T-007) over simulated hours: a continuous carrier split into 1 s boxes
//! with a gain change every 30 min stays one track and one burst, with bounded memory, and the
//! tracker allocates nothing in steady state (alongside a periodic burst emitter, drains included).
//! Own test binary: it installs a counting global allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use hk_core::ProvenanceHandle;
use hk_detect::{
    Candidate, CloseReason, DetectionRecord, TrackBatch, TrackEvent, Tracker, TrackerConfig,
};
use hk_model::{
    Detection, DetectionFlags, DetectionId, Provenance, SurveyId, TimeRange, Timestamp,
};

struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

fn note() {
    if COUNTING.try_with(Cell::get).unwrap_or(false) {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: forwards every call to the system allocator unchanged; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn counted<R>(f: impl FnOnce() -> R) -> R {
    COUNTING.with(|c| c.set(true));
    let r = f();
    COUNTING.with(|c| c.set(false));
    r
}

fn provenance(lna_db: f64) -> ProvenanceHandle {
    let json = serde_json::json!({
        "device_id": "synthetic:hk-detect-test",
        "tune": {"center_hz": 915e6, "sample_rate_hz": 10e6, "lna_db": lna_db,
                 "vga_db": 20.0, "amp_on": false, "bandwidth_hz": 7.5e6},
        "overload": false, "quantisation_limited": false,
        "clock_source": "internal", "clock_locked": true, "timestamp_method": "synthetic",
        "timestamp_error_budget_ns": 0,
    });
    ProvenanceHandle::new(serde_json::from_value::<Provenance>(json).unwrap())
}

const FRAME_NS: i64 = 100_000_000; // 100 ms frames keep hours of stream fast
const SPF: u64 = 1_000_000;

#[allow(clippy::too_many_arguments)]
fn rec(
    prov: &ProvenanceHandle,
    segment: u64,
    frame0: u64,
    frames: u64,
    fc: f64,
    obw: f64,
    close: CloseReason,
    continues: bool,
) -> DetectionRecord {
    let bin = 5e3;
    let nb = (obw / bin).ceil().max(1.0) as usize;
    let lo = fc - nb as f64 * bin / 2.0;
    DetectionRecord {
        detection: Detection {
            id: DetectionId::new(),
            survey_id: SurveyId::new(),
            time: TimeRange::new(
                Timestamp::from_unix_nanos(frame0 as i64 * FRAME_NS),
                Timestamp::from_unix_nanos((frame0 + frames) as i64 * FRAME_NS),
            ),
            f_center_hz: fc,
            obw_hz: obw,
            xdb_bandwidth_hz: None,
            xdb_level_db: None,
            snr_peak_db: 20.0,
            snr_mean_db: 15.0,
            peak_level_dbfs: -40.0,
            peak_level_dbm: None,
            sk: None,
            clip_count: 0,
            detector_version: "test".into(),
            provenance_ref: prov.id(),
            flags: DetectionFlags::default(),
        },
        provenance: prov.clone(),
        segment,
        bins: 100..100 + nb,
        f_lo_hz: lo,
        f_hi_hz: lo + nb as f64 * bin,
        frames: frame0..frame0 + frames,
        samples: frame0 * SPF..(frame0 + frames) * SPF,
        pixels: nb as u64 * frames,
        close,
        continues,
        candidate: Candidate::Unconfirmed,
        image: None,
        spur_harmonic_hz: None,
        merged_boxes: 1,
        inconclusive: false,
    }
}

#[test]
fn continuous_carrier_over_hours_one_track_bounded_memory_no_steady_allocation() {
    let hours = 6u64;
    let frames_total = hours * 3600 * 10;
    let seg_frames = 30 * 60 * 10; // gain change every 30 min
    let box_frames = 10; // 1 s max-duration boxes
    let (fcar, fb) = (914.0e6, 916.0e6);
    let provs = [provenance(16.0), provenance(24.0)];
    let mut tr = Tracker::new(TrackerConfig::default());
    let mut events: Vec<TrackEvent> = Vec::with_capacity(1024);
    let mut batch = TrackBatch::new();
    let mut steady_allocs = 0usize;
    let mut mem_at_1h = 0usize;
    let mut box_start = 0u64;
    let mut records = 0u64;
    for frame in 0..frames_total {
        let segment = frame / seg_frames + 1;
        let prov = &provs[(segment % 2) as usize];
        let seg_start = (segment - 1) * seg_frames;
        let steady = frame >= 36_000 / 6; // after 10 min
        let mut due: [Option<DetectionRecord>; 2] = [None, None];
        let last_in_segment = frame + 1 == seg_start + seg_frames || frame + 1 == frames_total;
        // Carrier: a box ends every 1 s (continues) or at the segment end (transition).
        if frame + 1 - box_start == box_frames || last_in_segment {
            let (close, cont) = if last_in_segment {
                if frame + 1 == frames_total {
                    (CloseReason::EndOfStream, false)
                } else {
                    (CloseReason::Transition, false)
                }
            } else {
                (CloseReason::MaxDuration, true)
            };
            due[0] = Some(rec(
                prov,
                segment,
                box_start,
                frame + 1 - box_start,
                fcar,
                10e3,
                close,
                cont,
            ));
            box_start = frame + 1;
        }
        // A 300 ms burst every 5 s beside it.
        if frame % 50 == 5 && frame > seg_start && frame + 3 < seg_start + seg_frames {
            due[1] = Some(rec(
                prov,
                segment,
                frame - 3,
                3,
                fb,
                20e3,
                CloseReason::Ended,
                false,
            ));
        }
        let allocs_before = ALLOCATIONS.load(Ordering::Relaxed);
        counted(|| {
            for r in due.iter().flatten() {
                tr.push_detection(r, &mut |e| events.push(e));
                records += 1;
            }
            tr.observe(
                segment,
                frame * SPF..(frame + 1) * SPF,
                TimeRange::new(
                    Timestamp::from_unix_nanos(frame as i64 * FRAME_NS),
                    Timestamp::from_unix_nanos((frame + 1) as i64 * FRAME_NS),
                ),
                &mut |e| events.push(e),
            );
            if frame % 600 == 0 {
                tr.drain_into(&mut batch);
                batch.upserts.clear();
                batch.links.clear();
                events.clear();
            }
        });
        if steady {
            steady_allocs += ALLOCATIONS.load(Ordering::Relaxed) - allocs_before;
        }
        assert!(
            tr.live_tracks() <= 2,
            "frame {frame}: {} live",
            tr.live_tracks()
        );
        if frame == 36_000 {
            mem_at_1h = tr.memory_bytes();
        }
    }
    let mem_end = tr.memory_bytes();
    let stats = tr.stats();
    events.clear();
    tr.finish(&mut |e| events.push(e));
    let closed: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            TrackEvent::Closed(s) => Some(s),
            _ => None,
        })
        .collect();
    eprintln!(
        "hours: {records} records, memory {mem_at_1h} → {mem_end} bytes, steady allocations {steady_allocs}, stats {stats:?}"
    );
    assert_eq!(steady_allocs, 0, "no steady-state allocation");
    assert_eq!(mem_end, mem_at_1h, "memory bounded");
    assert_eq!(closed.len(), 2, "carrier + burst emitter");
    let car = closed
        .iter()
        .find(|s| (s.track.f_center_hz - fcar).abs() < 5e3)
        .unwrap();
    assert_eq!(car.burst_count, 1, "one burst for hours");
    assert_eq!(
        car.segments as u64,
        hours * 2 - 1,
        "a boundary per gain change"
    );
    assert!((car.on_time_s - (hours * 3600) as f64).abs() < 1.0);
    assert!(car.track.timing.duty_cycle.unwrap() > 0.999);
    let bur = closed
        .iter()
        .find(|s| (s.track.f_center_hz - fb).abs() < 5e3)
        .unwrap();
    let p = bur.period.unwrap();
    assert!((p.period_s - 5.0).abs() < 0.05, "{p:?}");
    assert!((bur.track.timing.duty_cycle.unwrap() - 0.06).abs() < 0.005);
}
