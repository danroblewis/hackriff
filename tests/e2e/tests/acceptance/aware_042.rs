//! AWARE-042 (T3/T4): occupancy over hours, answered by the region-over-time history query.
//!
//! The `occupancy_multi_hour` synth draws a 3 h burst schedule for 8 NBFM channels (12.5 kHz
//! raster at 446 MHz, diurnal rate profile from 06:00 UTC) and renders IQ on demand for chosen
//! windows. Three 30 s windows, one in the middle of each hour, are rendered and joined into one
//! multi-capture SigMF recording (each capture keeps its own `core:datetime`, so the replay jumps
//! through the three hours), and that recording is replayed once through the composed pipeline.
//! Its history reader folds every frame into the SpectrumTile pyramid.
//!
//! Expected values are computed exactly from the schedule restricted to the rendered windows
//! (interval arithmetic): per-channel occupancy, the burst-length distribution (bursts clipped to
//! the windows) and the hour-of-window profile (occupancy per UTC hour). They must match the
//! pyramid's region-over-time query (`Pyramid::query` → `channel_summaries`) within the
//! tolerances below, which reflect the level-0 grid (1 s × 6.25 kHz cells).

use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::sigmf::{Datatype, SigmfMeta};
use hk_model::{FreqRange, Region, TimeRange};
use hk_store::history::burst_histogram;
use hk_store::{RegionQuery, Resolution};
use serde_json::{Value, json};

use crate::blind::{assert_truth_found, replay_config, start};
use crate::common::*;

const AWARE_042: &str = "AWARE-042";
const WINDOW_S: f64 = 30.0;
const WINDOW_STARTS_S: [f64; 3] = [1800.0, 5400.0, 9000.0];
/// Occupancy tolerance (fraction of time): absolute.
const OCC_TOL: f64 = 0.05;
/// Burst-duration histogram tolerance (fraction of bursts per bin): absolute.
const HIST_TOL: f64 = 0.15;
const HIST_EDGES_S: [f64; 6] = [0.0, 2.0, 5.0, 10.0, 20.0, f64::INFINITY];

fn f(v: &Value, k: &str) -> f64 {
    v[k].as_f64()
        .unwrap_or_else(|| panic!("[{AWARE_042}] schedule field {k} missing"))
}

fn overlap(a0: f64, a1: f64, b0: f64, b1: f64) -> f64 {
    (a1.min(b1) - a0.max(b0)).max(0.0)
}

#[test]
fn aware_042_occupancy_burst_lengths_and_hour_profile_via_region_over_time_query() {
    let starts = WINDOW_STARTS_S
        .iter()
        .map(|s| format!("{s}"))
        .collect::<Vec<_>>()
        .join(",");
    let out = synth_or_skip!(
        SynthRequest::new("occupancy_multi_hour")
            .seed(42)
            .param("window_starts_s", starts)
            .param("window_duration_s", WINDOW_S)
            .param("rates_per_hour", 240)
    );
    let schedule = out.file_json("schedule.json").unwrap();
    let fixtures = out.fixtures().unwrap();
    assert_eq!(fixtures.len(), WINDOW_STARTS_S.len());

    // Join the windows into one multi-capture recording.
    let src = TempDir::new("a042src");
    let mut meta: SigmfMeta = fixtures[0].meta.clone();
    assert_eq!(meta.global.datatype, Datatype::Ci8);
    meta.captures.clear();
    meta.annotations.clear();
    // The device's clock is its sample counter (T-049), so each window's `core:global_index`
    // places it in the 3 h span: the hour between windows is a recording gap the mock serves as
    // an overrun of that duration, exactly as a radio whose stream was interrupted.
    let fs = fixtures[0].sample_rate;
    let mut data = Vec::new();
    for (fx, window_s) in fixtures.iter().zip(WINDOW_STARTS_S) {
        let bytes = std::fs::read(fx.data_path()).unwrap();
        let offset = (data.len() / 2) as u64;
        for cap in &fx.meta.captures {
            let mut c = cap.clone();
            c.extra.insert(
                "core:global_index".into(),
                json!(((window_s - WINDOW_STARTS_S[0]) * fs).round() as u64 + cap.sample_start),
            );
            c.sample_start += offset;
            meta.captures.push(c);
        }
        data.extend_from_slice(&bytes);
    }
    let joined = src.0.join("occupancy.sigmf-meta");
    meta.write(&joined).unwrap();
    std::fs::write(src.0.join("occupancy.sigmf-data"), &data).unwrap();

    let dir = TempDir::new("a042");
    let (cfg, replay) = replay_config(&dir.0, &joined, json!({}), hk_core::Pacing::Unpaced);
    let handle = start(cfg, replay);
    let product = handle.floor_product();
    let s = finish(handle);
    // The hour-long recording gaps reach the pipeline as device overruns (GAP). Lossless readers
    // lose nothing beyond the true gap (T-072: the ring used to treat a reader parked at the index
    // jump as lapped and resynced it past up to half a ring of retained post-gap samples).
    assert_eq!(
        s.always_on_lost_samples, 0,
        "[{AWARE_042}] readers lost samples at the recording gaps"
    );
    let gaps = (WINDOW_STARTS_S.len() - 1) as u64;
    assert_eq!(
        s.counter("/readers/detect/gap_samples"),
        gaps * ((WINDOW_STARTS_S[1] - WINDOW_STARTS_S[0] - WINDOW_S) * fs).round() as u64,
        "[{AWARE_042}] the gaps are reported exactly"
    );
    assert!(s.counter("/history/tiles_written") > 0);
    // Every rendered burst of every window detected blind (T-047).
    for fx in &fixtures {
        assert_truth_found(AWARE_042, &dir.0, fx, 0.0, true);
    }

    // Truth restricted to the rendered windows.
    let span_s = f(&schedule, "span_s");
    let t_start = hk_core::source::sigmf_replay::parse_sigmf_datetime(
        schedule["start_utc"].as_str().unwrap(),
    )
    .unwrap();
    let start_hour_utc = (t_start.as_unix_nanos() / 1_000_000_000).rem_euclid(86_400) / 3600;
    let channels = schedule["channels"].as_array().unwrap();
    let bursts = schedule["bursts"].as_array().unwrap();
    let windows: Vec<(f64, f64)> = WINDOW_STARTS_S.iter().map(|&w| (w, w + WINDOW_S)).collect();
    let exposure = WINDOW_S * windows.len() as f64;
    struct Truth {
        occupancy: f64,
        per_window: Vec<f64>,
        clipped: Vec<f64>,
    }
    let truth: Vec<Truth> = (0..channels.len())
        .map(|c| {
            let mine: Vec<(f64, f64)> = bursts
                .iter()
                .filter(|b| b["channel"].as_u64() == Some(c as u64))
                .map(|b| (f(b, "start_s"), f(b, "start_s") + f(b, "duration_s")))
                .collect();
            let per_window: Vec<f64> = windows
                .iter()
                .map(|&(w0, w1)| {
                    mine.iter()
                        .map(|&(b0, b1)| overlap(b0, b1, w0, w1))
                        .sum::<f64>()
                        / WINDOW_S
                })
                .collect();
            let clipped = windows
                .iter()
                .flat_map(|&(w0, w1)| {
                    mine.iter()
                        .map(move |&(b0, b1)| overlap(b0, b1, w0, w1))
                        .filter(|d| *d > 0.0)
                })
                .collect();
            Truth {
                occupancy: per_window.iter().sum::<f64>() * WINDOW_S / exposure,
                per_window,
                clipped,
            }
        })
        .collect();

    // The region-over-time query over the whole 3 h span.
    let chans: Vec<FreqRange> = channels
        .iter()
        .map(|ch| FreqRange::centered(f(ch, "center_hz"), 12_500.0))
        .collect();
    let p = product.lock().unwrap();
    let grid = p
        .uncalibrated_pyramid()
        .query(&RegionQuery {
            freq: FreqRange::new(446.0e6, 446.1e6),
            time: TimeRange::new(t_start, t_start.saturating_add_nanos((span_s * 1e9) as i64)),
            resolution: Resolution::Level(0),
        })
        .unwrap();
    let summaries = grid.channel_summaries(&chans);
    let repo = repo(&dir.0);
    let mut all_est = Vec::new();
    let mut all_truth = Vec::new();
    let mut worst = [0.0f64; 2];
    for (c, (sum, t)) in summaries.iter().zip(&truth).enumerate() {
        let occ = sum.occupancy_fraction.unwrap_or(f64::NAN);
        let hours: Vec<Option<f64>> = (0..windows.len())
            .map(|w| {
                sum.hour_of_day
                    [(start_hour_utc as usize + (WINDOW_STARTS_S[w] / 3600.0) as usize) % 24]
            })
            .collect();
        eprintln!(
            "[{AWARE_042}] ch{c} {:.5} MHz: occupancy {occ:.3} (truth {:.3}), observed {:.0} s, \
             hour profile {:?} (truth {:?}), {} bursts {:?} (truth {:?})",
            chans[c].center_hz() / 1e6,
            t.occupancy,
            sum.observed_s,
            hours
                .iter()
                .map(|h| h.map(|v| (v * 1000.0).round() / 1000.0))
                .collect::<Vec<_>>(),
            t.per_window
                .iter()
                .map(|v| (v * 1000.0).round() / 1000.0)
                .collect::<Vec<_>>(),
            sum.bursts_s.len(),
            sum.bursts_s
                .iter()
                .map(|v| (v * 10.0).round() / 10.0)
                .collect::<Vec<_>>(),
            t.clipped
                .iter()
                .map(|v| (v * 10.0).round() / 10.0)
                .collect::<Vec<_>>(),
        );
        assert!(
            (sum.observed_s - exposure).abs() <= 3.0 * windows.len() as f64,
            "[{AWARE_042}] ch{c}: observed {} s of {exposure} s",
            sum.observed_s
        );
        worst[0] = worst[0].max((occ - t.occupancy).abs());
        assert!(
            (occ - t.occupancy).abs() <= OCC_TOL,
            "[{AWARE_042}] ch{c}: occupancy {occ:.3} vs truth {:.3}",
            t.occupancy
        );
        for (w, (got, want)) in hours.iter().zip(&t.per_window).enumerate() {
            let got = got.unwrap_or_else(|| panic!("[{AWARE_042}] ch{c}: hour {w} unobserved"));
            worst[1] = worst[1].max((got - want).abs());
            assert!(
                (got - want).abs() <= OCC_TOL,
                "[{AWARE_042}] ch{c}: hour-of-window {w} occupancy {got:.3} vs truth {want:.3}"
            );
        }
        if t.occupancy > 0.05 {
            let dets = repo
                .detections_in_region(&Region::new(
                    FreqRange::centered(chans[c].center_hz(), 12_500.0),
                    ever(),
                ))
                .unwrap();
            assert!(
                !dets.is_empty(),
                "[{AWARE_042}] ch{c}: busy channel without detections"
            );
        }
        all_est.extend(sum.bursts_s.iter().copied());
        all_truth.extend(t.clipped.iter().copied());
    }
    // Hours outside the windows stay unobserved.
    let h_other = (start_hour_utc as usize + 3) % 24;
    assert!(summaries.iter().all(|s| s.hour_of_day[h_other].is_none()));

    // Pooled burst-length distribution.
    let est = burst_histogram(&all_est, &HIST_EDGES_S);
    let tru = burst_histogram(&all_truth, &HIST_EDGES_S);
    let (ne, nt) = (all_est.len().max(1) as f64, all_truth.len().max(1) as f64);
    eprintln!(
        "[{AWARE_042}] burst-length histogram {HIST_EDGES_S:?}: estimated {est:?} ({ne}), truth {tru:?} ({nt}); \
         worst occupancy error {:.3}, worst hour-profile error {:.3}",
        worst[0], worst[1]
    );
    assert!(
        nt >= 20.0,
        "[{AWARE_042}] too few bursts in the windows: {nt}"
    );
    for (i, (&e, &t)) in est.iter().zip(&tru).enumerate() {
        assert!(
            (e as f64 / ne - t as f64 / nt).abs() <= HIST_TOL,
            "[{AWARE_042}] burst-length bin {i}: {:.3} vs truth {:.3}",
            e as f64 / ne,
            t as f64 / nt
        );
    }
}
