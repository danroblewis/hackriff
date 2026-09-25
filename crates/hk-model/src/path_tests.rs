//! T-897: path derivation over hand-built detections. Each test states the truth the detections
//! were built from and asserts the path against that truth — never against the numbers that came
//! out. The blind, through-the-device version is `hk-pipeline/tests/paths_blind.rs`.

use super::*;
use crate::detection::DetectionFlags;

const T0_NS: i64 = 1_790_000_000_000_000_000;

fn ns(s: f64) -> Timestamp {
    Timestamp::from_unix_nanos(T0_NS + (s * 1e9).round() as i64)
}

fn secs(t: Timestamp) -> f64 {
    (t.as_unix_nanos() - T0_NS) as f64 / 1e9
}

fn det(t0: f64, t1: f64, fc: f64, obw: f64) -> Detection {
    Detection {
        id: DetectionId::new(),
        survey_id: SurveyId::from_uuid(uuid::Uuid::nil()),
        time: TimeRange::new(ns(t0), ns(t1)),
        f_center_hz: fc,
        obw_hz: obw,
        xdb_bandwidth_hz: None,
        xdb_level_db: None,
        snr_peak_db: 20.0,
        snr_mean_db: 15.0,
        peak_level_dbfs: -20.0,
        peak_level_dbm: None,
        sk: None,
        clip_count: 0,
        detector_version: "test".into(),
        provenance_ref: ProvenanceId::from_uuid(uuid::Uuid::nil()),
        flags: DetectionFlags::default(),
    }
}

/// A linear chirp `f(t) = f0 + rate·(t - t0)` over `[t0, t0 + n·seg]`, cut into `n` boxes the way
/// the detector's `max_duration_s` cuts a long component: each box is the bounding box of its
/// segment's sweep plus the tone's own `inst` width.
fn chirp(t0: f64, n: usize, seg: f64, f0: f64, rate: f64, inst: f64) -> Vec<Detection> {
    (0..n)
        .map(|k| {
            let (a, b) = (t0 + k as f64 * seg, t0 + (k + 1) as f64 * seg);
            let (fa, fb) = (f0 + rate * (a - t0), f0 + rate * (b - t0));
            det(a, b, 0.5 * (fa + fb), (fb - fa).abs() + inst)
        })
        .collect()
}

fn on_line(p: &TracedPath, truth: impl Fn(f64) -> f64, tol_hz: f64) {
    for v in &p.vertices {
        let t = secs(v.t);
        assert!(
            (v.f_hz - truth(t)).abs() <= tol_hz,
            "vertex ({t:.3} s, {:.0} Hz) is {:.0} Hz off the truth line",
            v.f_hz,
            v.f_hz - truth(t)
        );
    }
}

#[test]
fn a_segmented_linear_chirp_traces_one_chirp_on_its_truth_line_end_to_end() {
    let d = chirp(0.0, 3, 1.0, 100e3, 30e3, 1e3);
    let paths = derive_paths(&d, &PathConfig::default());
    assert_eq!(paths.len(), 1, "{paths:?}");
    let p = &paths[0];
    assert_eq!(p.kind, PathKind::Chirp);
    assert_eq!(p.ramps, 1);
    on_line(p, |t| 100e3 + 30e3 * t, 600.0);
    // Its whole time extent: the first vertex at the chirp's start, the last at its end.
    assert_eq!(secs(p.vertices[0].t), 0.0);
    assert_eq!(secs(p.vertices.last().unwrap().t), 3.0);
    assert_eq!(p.vertices[0].at, VertexAt::Start);
    assert_eq!(p.vertices.last().unwrap().at, VertexAt::End);
    assert!(
        p.vertices.windows(2).all(|w| w[0].t <= w[1].t),
        "ordered by time"
    );
    let rate = p.rate_hz_per_s.unwrap();
    assert!((rate - 30e3).abs() < 300.0, "rate {rate}");
    // Provenance names every member, and each vertex names its own detection.
    assert_eq!(
        p.provenance.detections,
        d.iter().map(|d| d.id).collect::<Vec<_>>()
    );
    assert_eq!(p.provenance.method, PATH_METHOD);
    for v in &p.vertices {
        assert!(p.provenance.detections.contains(&v.detection));
    }
}

#[test]
fn a_down_chirp_is_a_chirp_too_and_its_rate_is_negative() {
    let d = chirp(5.0, 4, 0.05, 200e3, -400e3, 2e3);
    let paths = derive_paths(&d, &PathConfig::default());
    assert_eq!(paths.len(), 1, "{paths:?}");
    assert_eq!(paths[0].kind, PathKind::Chirp);
    on_line(&paths[0], |t| 200e3 - 400e3 * (t - 5.0), 1_100.0);
    assert!(paths[0].rate_hz_per_s.unwrap() < 0.0);
}

#[test]
fn an_end_vertex_never_leaves_its_own_box() {
    // A ladder whose last rung is short and whose slope would carry the end past its box.
    let mut d = chirp(0.0, 3, 1.0, 100e3, 30e3, 1e3);
    d.push(det(3.0, 3.1, 191e3, 2e3));
    let paths = derive_paths(&d, &PathConfig::default());
    let p = &paths[0];
    let last = p.vertices.last().unwrap();
    let owner = d.iter().find(|x| x.id == last.detection).unwrap();
    assert!(last.f_hz >= owner.f_center_hz - owner.obw_hz / 2.0);
    assert!(last.f_hz <= owner.f_center_hz + owner.obw_hz / 2.0);
}

#[test]
fn a_steady_carrier_cut_into_segments_draws_no_path() {
    // Jitter of a bin either way, the width of a tone: the detector's view of a carrier.
    let jitter = [0.0, 700.0, -600.0, 650.0, -700.0, 500.0];
    let d: Vec<Detection> = jitter
        .iter()
        .enumerate()
        .map(|(k, j)| det(k as f64, k as f64 + 1.0, 50e3 + j, 3e3))
        .collect();
    assert!(derive_paths(&d, &PathConfig::default()).is_empty());
}

/// Contiguous `dwell`-second hops over `chans` in the given order, starting at `t0`.
fn hops(t0: f64, dwell: f64, order: &[f64]) -> Vec<Detection> {
    order
        .iter()
        .enumerate()
        .map(|(k, &f)| det(t0 + k as f64 * dwell, t0 + (k + 1) as f64 * dwell, f, 3e3))
        .collect()
}

const HOP_ORDER: [f64; 10] = [
    130e3, 170e3, 150e3, 190e3, 130e3, 150e3, 170e3, 190e3, 150e3, 130e3,
];

#[test]
fn a_hop_sequence_traces_every_dwell_as_a_staircase() {
    let d = hops(2.0, 0.034, &HOP_ORDER);
    let paths = derive_paths(&d, &PathConfig::default());
    assert_eq!(paths.len(), 1, "{paths:?}");
    let p = &paths[0];
    assert_eq!(p.kind, PathKind::Hop);
    assert_eq!(p.hops, 10);
    assert_eq!(p.channels_hz, vec![130e3, 150e3, 170e3, 190e3]);
    assert_eq!(p.vertices.len(), 20);
    for (k, &f) in HOP_ORDER.iter().enumerate() {
        let (a, b) = (&p.vertices[2 * k], &p.vertices[2 * k + 1]);
        assert_eq!((a.f_hz, b.f_hz), (f, f), "hop {k}");
        assert!((secs(a.t) - (2.0 + k as f64 * 0.034)).abs() < 1e-6);
        assert!((secs(b.t) - (2.0 + (k + 1) as f64 * 0.034)).abs() < 1e-6);
        assert_eq!((a.at, b.at), (VertexAt::Start, VertexAt::End));
    }
    assert!(p.rate_hz_per_s.is_none());
}

#[test]
fn stray_bursts_and_a_carrier_beside_a_hopper_stay_out_of_its_path() {
    let mut d = hops(2.0, 0.034, &HOP_ORDER);
    let hop_ids: Vec<DetectionId> = d.iter().map(|d| d.id).collect();
    // Short bursts abutting two of the dwells in time, on other frequencies.
    d.push(det(2.102, 2.114, -90e3, 3e3));
    d.push(det(2.204, 2.216, -30e3, 3e3));
    // A carrier the whole time.
    for k in 0..4 {
        d.push(det(1.0 + k as f64, 2.0 + k as f64, 50e3, 3e3));
    }
    let paths = derive_paths(&d, &PathConfig::default());
    assert_eq!(paths.len(), 1, "{paths:?}");
    assert_eq!(paths[0].provenance.detections, hop_ids);
}

#[test]
fn two_channels_taking_turns_are_not_a_hop_sequence() {
    let order = [130e3, 150e3, 130e3, 150e3, 130e3, 150e3, 130e3, 150e3];
    assert!(derive_paths(&hops(0.0, 0.034, &order), &PathConfig::default()).is_empty());
}

#[test]
fn too_few_hops_are_not_a_sequence() {
    let order = [130e3, 150e3, 170e3, 190e3];
    assert!(derive_paths(&hops(0.0, 0.034, &order), &PathConfig::default()).is_empty());
}

#[test]
fn a_sawtooth_is_one_sweep_of_three_ramps_with_the_flyback_in_its_route() {
    let mut d = Vec::new();
    for k in 0..3 {
        d.extend(chirp(0.3 + 3.0 * k as f64, 3, 1.0, 20e3, 25e3, 1e3));
    }
    let paths = derive_paths(&d, &PathConfig::default());
    assert_eq!(paths.len(), 1, "{paths:?}");
    let p = &paths[0];
    assert_eq!(p.kind, PathKind::Sweep);
    assert_eq!(p.ramps, 3);
    // Every vertex on the ramp its own detection belongs to (a flyback instant is the end of one
    // ramp and the start of the next, at the top and the bottom of the same time).
    for v in &p.vertices {
        let owner = d.iter().find(|x| x.id == v.detection).unwrap();
        let k = ((secs(owner.time.start) - 0.3) / 3.0).floor();
        let truth = 20e3 + 25e3 * (secs(v.t) - 0.3 - 3.0 * k);
        assert!((v.f_hz - truth).abs() <= 600.0, "{v:?} vs {truth}");
    }
    // Each ramp contributes its own start and end vertex: the flyback is drawn at its time.
    let starts = p
        .vertices
        .iter()
        .filter(|v| v.at == VertexAt::Start)
        .count();
    assert_eq!(starts, 3);
    assert!((p.rate_hz_per_s.unwrap() - 25e3).abs() < 300.0);
}

#[test]
fn a_triangle_sweep_is_one_sweep_of_two_ramps() {
    let mut d = chirp(0.0, 3, 1.0, 20e3, 30e3, 1e3);
    d.extend(chirp(3.0, 3, 1.0, 110e3, -30e3, 1e3));
    let paths = derive_paths(&d, &PathConfig::default());
    assert_eq!(paths.len(), 1, "{paths:?}");
    assert_eq!(paths[0].kind, PathKind::Sweep);
    assert_eq!(paths[0].ramps, 2);
}

#[test]
fn a_spur_or_image_ladder_draws_nothing() {
    for flag in 0..2 {
        let mut d = chirp(0.0, 3, 1.0, 100e3, 30e3, 1e3);
        for x in &mut d {
            if flag == 0 {
                x.flags.spur_candidate = true;
            } else {
                x.flags.image_candidate = true;
            }
        }
        assert!(derive_paths(&d, &PathConfig::default()).is_empty());
    }
}

#[test]
fn the_answer_is_a_function_of_the_set_not_its_order() {
    let mut d = chirp(0.0, 3, 1.0, 100e3, 30e3, 1e3);
    d.extend(hops(4.0, 0.034, &HOP_ORDER));
    let a = derive_paths(&d, &PathConfig::default());
    d.reverse();
    d.swap(1, 7);
    let b = derive_paths(&d, &PathConfig::default());
    assert_eq!(a, b);
    assert_eq!(a.len(), 2);
    assert_eq!(
        a[0].id(),
        format!("chirp:{}", a[0].provenance.detections[0])
    );
    assert!(a[1].id().starts_with("hop:"));
}
