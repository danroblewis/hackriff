//! AWARE-006 / T-005 third review: floor-tracker episodes through the T-020 Anomaly lifecycle.
//! Merges, splits and widening keep the still-elevated bins covered by open anomalies
//! (≥ 95 %), keep one open anomaly per widening rise, and keep Explanations attached to an open
//! anomaly. T-029: open anomalies never overlap, and a structured region merged into a
//! noise-like episode is not covered.
#![allow(clippy::single_range_in_vec_init)] // elevated regions are lists of ranges

#[path = "../../hk-dsp/tests/common/mod.rs"]
mod common;
#[path = "../../hk-dsp/tests/floor_common/mod.rs"]
mod floor_common;

use std::collections::BTreeSet;
use std::ops::Range;

use common::*;
use floor_common::*;
use hk_context::{FloorAnomalies, FloorAnomalyConfig};
use hk_core::Discontinuity;
use hk_dsp::floor::{FloorConfig, FloorEvent, NoiseFloorTracker};
use hk_model::{
    Cause, CorrelationType, Explanation, ExplanationId, FreqRange, Region, Repository, TimeRange,
    Timestamp,
};

const BINS: usize = 4096;
const FS: f64 = 4e6;
const FC: f64 = 1575.42e6;
/// Extents resolve to half a 64-bin block hop: coverage is scored on each elevated range less
/// this at both ends, and open bins count as stale only beyond this outside every range.
const EDGE: usize = 64;

fn scale(p: &mut [f32], r: Range<usize>, db: f64) {
    let g = 10f64.powf(db / 10.0) as f32;
    for v in &mut p[r] {
        *v *= g;
    }
}

struct Outcome {
    coverage: f64,
    stale_bins: usize,
    open: usize,
    unexplained_open: usize,
}

/// Runs the tracker over `scene`, applies every event to the lifecycle (explaining each newly
/// opened anomaly the way the correlator would), and measures coverage of `elevated`.
fn run(
    name: &str,
    secs: f64,
    seed: u64,
    mut scene: impl FnMut(f64, &mut [f32], &mut [f32]),
    elevated: &[Range<usize>],
) -> Outcome {
    let prov = provenance_with(FC, FS, 16.0);
    let mut src = GammaFrames::new(BINS, 10, prov, seed);
    let mut pool = GammaPool::new(10, seed + 7);
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut frame = src.empty_frame();
    frame.spectrum.sk = vec![1.0; BINS];
    let (mut prof, mut sk) = (vec![1.0f32; BINS], vec![1.0f32; BINS]);
    let mut events: Vec<FloorEvent> = Vec::new();
    while (src.seq as f64) * src.frame_period_s() < secs {
        let t = src.seq as f64 * src.frame_period_s();
        prof.fill(1.0);
        sk.fill(1.0);
        scene(t, &mut prof, &mut sk);
        src.fill_pooled(&mut frame, &prof, Discontinuity::NONE, &mut pool);
        frame.spectrum.sk.copy_from_slice(&sk);
        tracker.update(&frame, |e| events.push(e.clone()));
    }
    let mut repo = Repository::open_in_memory().unwrap();
    let mut life = FloorAnomalies::new(FloorAnomalyConfig::new("coverage:1")).unwrap();
    eprintln!("=== {name}");
    for e in &events {
        let r = life.on_floor_event(&mut repo, e).unwrap();
        let successors: BTreeSet<_> = r.superseded.iter().map(|s| s.1).collect();
        for id in r.opened.iter().filter(|id| !successors.contains(id)) {
            repo.insert_explanation(&Explanation {
                id: ExplanationId::new(),
                anomaly_ref: *id,
                cause: Cause::Unexplained,
                correlation_type: CorrelationType::TimeCoincidence,
                score: 0.5,
                evidence: vec![],
                supersedes: None,
                provisional: false,
                rule_version: "coverage-test@1".into(),
                t: e.confirmed_t.host_time,
            })
            .unwrap();
        }
        // T-029: open anomalies never overlap, after every signal.
        let freqs: Vec<FreqRange> = life
            .open_anomalies()
            .iter()
            .map(|id| repo.anomaly(*id).unwrap().region.freq)
            .collect();
        for (k, a) in freqs.iter().enumerate() {
            for b in &freqs[k + 1..] {
                assert!(
                    a.hi_hz.min(b.hi_hz) <= a.lo_hz.max(b.lo_hz),
                    "{name}: open anomalies overlap after {:?} #{}: {a:?} / {b:?}",
                    e.kind,
                    e.episode
                );
            }
        }
        eprintln!(
            "  {:?} #{} {:?}{}{} bins {:?} change {:?} -> opened {} closed {} reparented {} superseded {}",
            e.kind,
            e.episode,
            e.class,
            e.merged_into
                .map_or(String::new(), |m| format!(" into #{m}")),
            e.split_from
                .map_or(String::new(), |m| format!(" from #{m}")),
            e.bins,
            e.change_bins,
            r.opened.len(),
            r.closed.len(),
            r.reparented.len(),
            r.superseded.len()
        );
    }
    let everything = Region::new(
        FreqRange::new(0.0, 7e9),
        TimeRange::new(
            Timestamp::from_unix_nanos(0),
            Timestamp::from_unix_nanos(i64::MAX / 2),
        ),
    );
    let open = life.open_anomalies();
    let mut covered = vec![false; BINS];
    let mut unexplained_open = 0;
    for a in repo.anomalies_in_region(&everything).unwrap() {
        if !open.contains(&a.id) {
            continue;
        }
        unexplained_open += usize::from(repo.explanations_for_anomaly(a.id).unwrap().is_empty());
        let bin = |hz: f64| ((hz - FC) / (FS / BINS as f64) + BINS as f64 / 2.0).round();
        let (lo, hi) = (bin(a.region.freq.lo_hz), bin(a.region.freq.hi_hz));
        eprintln!("  open anomaly over bins {lo}..{hi}");
        for c in covered
            .iter_mut()
            .take((hi.max(0.0) as usize).min(BINS))
            .skip(lo.max(0.0) as usize)
        {
            *c = true;
        }
    }
    let (mut el, mut cov, mut stale) = (0, 0, 0);
    for (b, &c) in covered.iter().enumerate() {
        let inside = elevated
            .iter()
            .any(|r| (r.start + EDGE..r.end - EDGE).contains(&b));
        let near = elevated
            .iter()
            .any(|r| (r.start.saturating_sub(EDGE)..r.end + EDGE).contains(&b));
        el += usize::from(inside);
        cov += usize::from(inside && c);
        stale += usize::from(!near && c);
    }
    let out = Outcome {
        coverage: cov as f64 / el.max(1) as f64,
        stale_bins: stale,
        open: open.len(),
        unexplained_open,
    };
    eprintln!(
        "  {} open anomalies; coverage of elevated bins {:.1} %, open bins not elevated {stale}, open anomalies without an explanation {unexplained_open}",
        out.open,
        100.0 * out.coverage
    );
    out
}

#[test]
fn aware_006_bridge_merge_keeps_coverage_and_explanations() {
    let o = run(
        "bridge merge (all NoiseLike), still elevated at the end",
        9.0,
        1,
        |t, p, _| {
            if t >= 1.0 {
                scale(p, 500..1200, 6.0);
                scale(p, 2000..2800, 6.0);
            }
            if t >= 3.0 {
                scale(p, 1100..2100, 6.0);
            }
        },
        &[500..2800],
    );
    assert!(o.coverage >= 0.95, "coverage {:.3}", o.coverage);
    assert_eq!(o.unexplained_open, 0, "explanations survive the merge");
}

#[test]
fn aware_006_structured_older_episode_merge_keeps_the_noise_like_anomaly() {
    let o = run(
        "Structured A (older) + NoiseLike B, NoiseLike bridge",
        9.0,
        2,
        |t, p, sk| {
            if t >= 1.0 {
                scale(p, 500..1200, 6.0);
                sk[500..1200].fill(1.6);
            }
            if t >= 2.0 {
                scale(p, 2000..2800, 6.0);
            }
            if t >= 4.0 {
                scale(p, 1100..2100, 6.0);
            }
        },
        // The noise-like elevation: B and the bridge beyond A (A is structured).
        &[1200..2800],
    );
    assert!(o.coverage >= 0.95, "coverage {:.3}", o.coverage);
    assert_eq!(
        o.stale_bins, 0,
        "T-029: the structured region is not covered by the noise-like anomaly"
    );
    assert_eq!(o.unexplained_open, 0);
}

#[test]
fn aware_006_widening_rise_stays_one_open_anomaly() {
    let o = run(
        "widening noise rise 1800..2300 growing 100 bins/s per side for 15 s",
        18.0,
        3,
        |t, p, _| {
            if t >= 1.0 {
                let g = ((t - 1.0).min(15.0) * 100.0) as usize;
                scale(p, 1800 - g..2300 + g, 6.0);
            }
        },
        &[300..3800],
    );
    assert_eq!(o.open, 1, "one widening rise, one open anomaly");
    assert!(o.coverage >= 0.95, "coverage {:.3}", o.coverage);
    assert_eq!(o.unexplained_open, 0);
}

#[test]
fn aware_006_vanished_bridge_shrinks_back_to_the_elevated_parts() {
    let o = run(
        "merge, then the bridge vanishes (A and B stay)",
        14.0,
        4,
        |t, p, _| {
            if t >= 1.0 {
                scale(p, 500..1200, 6.0);
                scale(p, 2000..2800, 6.0);
            }
            if (3.0..6.0).contains(&t) {
                scale(p, 1100..2100, 6.0);
            }
        },
        &[500..1200, 2000..2800],
    );
    assert!(o.coverage >= 0.95, "coverage {:.3}", o.coverage);
    assert!(
        o.stale_bins == 0,
        "open anomaly bins back at the floor: {}",
        o.stale_bins
    );
    assert_eq!(o.unexplained_open, 0);
}
