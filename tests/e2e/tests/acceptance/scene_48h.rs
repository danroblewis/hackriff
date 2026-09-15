//! T-125 (AWARE-042, AWARE-044): a 48 h occupancy scene replayed time-compressed through the mock
//! SDR on its own simulated clock.
//!
//! `occupancy_markov_scene` draws 48 h of channel activity (Markov channels, an hour-of-week
//! channel, a launch-like event, a boring wideband band, and a novelty emitter silent until hour
//! 30) and renders one short IQ window per observation-schedule revisit. The harness joins the
//! windows on the scene clock and one mock device serves them: between windows stream time jumps
//! (`GAP`), no silence is synthesised. The run is blind; the schedule is read only below.
//!
//! Asserted: the gaps reach the pipeline exactly; detections and inventory first/last seen lie in
//! the scene's windows (simulated time, spanning ~48 h, not the replay's wall time); history tiles
//! hold data at the start and the end of the 48 h and nothing between two windows; the novelty
//! emitter is detected, and never before simulated hour 30.

use std::time::Instant;

use hk_e2e::scene::SceneTruth;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::{FreqRange, Region, TimeRange, Timestamp};
use hk_store::{RegionQuery, Resolution};
use serde_json::json;

use crate::blind::{blind_scene, inventory_rows, start};
use crate::common::*;

const T125: &str = "T-125";
const HOUR_S: f64 = 3600.0;
const WINDOW_S: f64 = 0.5;
/// Detection-time slack around a window, s (frame and track extent).
const TIME_TOL_S: f64 = 1.0;

#[test]
fn scene_48h_replays_time_compressed_with_novelty_after_hour_30() {
    let out = synth_or_skip!(
        SynthRequest::new("occupancy_markov_scene")
            .seed(7)
            .param("span_hours", 48)
            .param("iq_windows_at_revisits", "true")
            .param("revisit_mean_gap_s", 1800)
            .param("window_duration_s", WINDOW_S)
    );
    let dir = TempDir::new("t125");
    let wall = Instant::now();
    let scene = blind_scene(&dir.0, &out, json!({}));
    let rec = scene.recording.clone();
    let handle = start(scene.cfg, scene.device);
    let product = handle.floor_product();
    let s = finish(handle);
    let wall_s = wall.elapsed().as_secs_f64();

    // The recording's own first-sample time (metadata, not truth) is the scene clock's anchor.
    let meta = hk_model::sigmf::SigmfMeta::read(&rec.meta).unwrap();
    let t_first = hk_core::source::sigmf_replay::parse_sigmf_datetime(
        meta.captures[0].datetime.as_deref().unwrap(),
    )
    .unwrap();

    // ---- Truth, for assertions only. ----
    let truth = SceneTruth::load(&out).unwrap();
    let w = &truth.window_starts_s;
    let t_scene = t_first.saturating_add_nanos(-((w[0] * 1e9).round() as i64));
    let at = |s: f64| t_scene.saturating_add_nanos((s * 1e9).round() as i64);
    let scene_s = |t: Timestamp| (t.as_unix_nanos() - t_scene.as_unix_nanos()) as f64 / 1e9;
    let in_a_window = |a: f64, b: f64| {
        w.iter()
            .any(|&ws| a <= ws + WINDOW_S + TIME_TOL_S && b >= ws - TIME_TOL_S)
    };
    eprintln!(
        "[{T125}] {} windows, {} IQ samples, simulated span {:.2} h, wall {wall_s:.1} s \
         (compression {:.0}x)",
        rec.windows,
        rec.samples,
        rec.simulated_span_s / HOUR_S,
        rec.simulated_span_s / wall_s
    );
    assert!(
        rec.simulated_span_s > 44.0 * HOUR_S,
        "[{T125}] the scene spans ~48 h"
    );

    // ---- The gaps reached the pipeline as the device reported them, and nothing was lost. ----
    assert_eq!(s.always_on_lost_samples, 0, "[{T125}] readers lost samples");
    assert_eq!(
        s.counter("/readers/detect/gap_samples"),
        rec.gap_samples,
        "[{T125}] the scene gaps are reported exactly"
    );
    assert!(s.counter("/history/tiles_written") > 0);

    // ---- Detections carry simulated times, inside the windows, spanning ~48 h. ----
    let repo = repo(&dir.0);
    let dets = repo
        .detections_in_region(&Region::new(FreqRange::new(0.0, 7.0e9), ever()))
        .unwrap();
    assert!(!dets.is_empty(), "[{T125}] no detections");
    for d in &dets {
        let (a, b) = (scene_s(d.time.start), scene_s(d.time.end));
        assert!(
            in_a_window(a, b),
            "[{T125}] detection at scene {a:.1}..{b:.1} s ({:.4} MHz) lies outside every window",
            d.f_center_hz / 1e6
        );
    }
    let first = dets
        .iter()
        .map(|d| scene_s(d.time.start))
        .fold(f64::INFINITY, f64::min);
    let last = dets
        .iter()
        .map(|d| scene_s(d.time.end))
        .fold(f64::NEG_INFINITY, f64::max);
    eprintln!(
        "[{T125}] {} detections from scene hour {:.2} to {:.2}",
        dets.len(),
        first / HOUR_S,
        last / HOUR_S
    );
    assert!(
        first < w[0] + WINDOW_S + TIME_TOL_S && last > w[w.len() - 1] - TIME_TOL_S,
        "[{T125}] detections span the first to the last window"
    );

    // ---- History tiles: data at both ends of the 48 h, nothing between two windows. ----
    let boring = truth.channel("boring").unwrap();
    let band = FreqRange::centered(boring.center_hz, boring.bandwidth_hz * 0.5);
    let observed = |a: f64, b: f64| {
        let p = product.lock().unwrap();
        let grid = p
            .uncalibrated_pyramid()
            .query(&RegionQuery {
                freq: band,
                time: TimeRange::new(at(a), at(b)),
                resolution: Resolution::Level(0),
            })
            .unwrap();
        grid.channel_summaries(&[band])[0].observed_s
    };
    let early = observed(0.0, w[0] + 60.0);
    let late = observed(w[w.len() - 1] - 1.0, truth.span_s);
    let (ga, gb) = w
        .windows(2)
        .map(|p| (p[0], p[1]))
        .max_by(|x, y| (x.1 - x.0).total_cmp(&(y.1 - y.0)))
        .unwrap();
    let between = observed(ga + WINDOW_S + 5.0, gb - 5.0);
    eprintln!(
        "[{T125}] history observed: first window {early:.1} s, last window {late:.1} s, \
         longest gap ({:.2} h..{:.2} h) {between:.1} s",
        ga / HOUR_S,
        gb / HOUR_S
    );
    assert!(
        early > 0.0,
        "[{T125}] no history tile data at the scene start"
    );
    assert!(late > 0.0, "[{T125}] no history tile data at scene hour 48");
    assert_eq!(
        between, 0.0,
        "[{T125}] history holds data between two windows"
    );

    // ---- Novelty: detected, never before simulated hour 30. ----
    let novelty = truth.channel("novelty").unwrap();
    let tol = novelty.bandwidth_hz / 2.0;
    let on_in_windows = truth
        .intervals(novelty.channel)
        .iter()
        .filter(|&&(a, b)| w.iter().any(|&ws| a < ws + WINDOW_S && b > ws))
        .count();
    assert!(
        on_in_windows > 0,
        "[{T125}] the scene never renders the novelty emitter"
    );
    let at_novelty: Vec<_> = dets
        .iter()
        .filter(|d| (d.f_center_hz - novelty.center_hz).abs() <= tol)
        .collect();
    let first_novelty = at_novelty
        .iter()
        .map(|d| scene_s(d.time.start))
        .fold(f64::INFINITY, f64::min);
    eprintln!(
        "[{T125}] novelty: {} detections, first at scene hour {:.2} (injected at {:.1} h, on in \
         {on_in_windows} windows)",
        at_novelty.len(),
        first_novelty / HOUR_S,
        truth.novelty_start_s / HOUR_S
    );
    assert!(
        !at_novelty.is_empty(),
        "[{T125}] the novelty emitter was never detected"
    );
    assert!(
        first_novelty >= truth.novelty_start_s - TIME_TOL_S,
        "[{T125}] novelty detected at scene hour {:.2}, before hour 30",
        first_novelty / HOUR_S
    );

    // ---- Inventory first/last seen are scene times. ----
    let rows = inventory_rows(&dir.0);
    assert!(!rows.is_empty(), "[{T125}] empty inventory");
    let t_scene_s = t_scene.as_unix_nanos() as f64 / 1e9;
    for r in &rows {
        let (f, l) = (
            r["first_seen_s"].as_f64().unwrap() - t_scene_s,
            r["last_seen_s"].as_f64().unwrap() - t_scene_s,
        );
        assert!(
            f >= w[0] - TIME_TOL_S && l <= truth.span_s + TIME_TOL_S,
            "[{T125}] inventory row seen {f:.1}..{l:.1} s outside the scene: {r}"
        );
        let fc = r["f_center_hz"].as_f64().unwrap_or(f64::NAN);
        if (fc - novelty.center_hz).abs() <= tol {
            assert!(
                f >= truth.novelty_start_s - TIME_TOL_S,
                "[{T125}] novelty inventory entry first seen at scene hour {:.2}",
                f / HOUR_S
            );
        }
    }
}
