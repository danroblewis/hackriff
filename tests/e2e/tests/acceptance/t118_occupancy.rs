//! T-118 (AWARE-042, PROP-023): blind occupancy (FCO/FBO/SRO, ADR-0012 §2) on the 48 h
//! `occupancy_markov_scene`, replayed time-compressed through the mock SDR (T-125).
//!
//! The run learns channels from its own detections, reads its history and computes one
//! `OccupancyStat` per learned channel and one for the band over the whole scene
//! (`OccupancyService::span_stats`, what `/api/occupancy?interval=span` answers). The query box is
//! the device's own tuning and the history's own time extent; the hidden schedule is read only for
//! the assertions.
//!
//! **Pass criterion (fixed before the first run):**
//! 1. every truth channel whose realized FCO is ≥ 5 % has a learned channel row with an `fco`;
//! 2. at least 5 truth channels are matched, and the realized FCO (`stats.per_channel[].fco_realized`)
//!    lies inside the row's 95 % Wilson interval for at least 75 % of matched channels;
//! 3. no phantom occupancy: a channel row whose extent holds no truth channel reports `fco` ≤ 5 %;
//! 4. a band row carries `fbo` and `sro`; 15-min rows were persisted and the plan version is ≥ 1.

use std::time::Instant;

use hk_e2e::scene::SceneTruth;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::attention::occupancy::OccupancySubject;
use hk_model::{FreqRange, TimeRange};
use hk_store::occupancy::{OccupancyQuery, SeriesInterval};
use serde_json::json;

use crate::blind::{blind_scene, start};
use crate::common::*;

const T118: &str = "T-118";

#[test]
fn t118_occupancy_fco_matches_hidden_truth_on_the_48h_markov_scene() {
    let out = synth_or_skip!(
        SynthRequest::new("occupancy_markov_scene")
            .seed(7)
            .param("span_hours", 48)
            .param("iq_windows_at_revisits", "true")
            .param("revisit_mean_gap_s", 900)
            .param("window_duration_s", 0.5)
    );
    let dir = TempDir::new("t118");
    let wall = Instant::now();
    let scene = blind_scene(&dir.0, &out, json!({}));
    let handle = start(scene.cfg, scene.device);
    let occ = handle.occupancy();
    let product = handle.floor_product();
    let counters = handle.counters();
    let s = finish(handle);
    assert_eq!(s.always_on_lost_samples, 0, "[{T118}] readers lost samples");

    // ---- Blind query box: the device's tuning and the history's own extent. ----
    let (center, rate) = counters.tune();
    let band = FreqRange::centered(center, 0.9 * rate);
    let end = product
        .lock()
        .unwrap()
        .uncalibrated_pyramid()
        .latest_frame_end()
        .expect("history holds frames");
    let span = TimeRange::new(
        end.saturating_add_nanos(-(48 * 3600 + 60) * 1_000_000_000),
        end.saturating_add_nanos(1_000_000_000),
    );
    let t = Instant::now();
    let rows = occ.span_stats(band, span).expect("span stats");
    let (plan_version, f_cell) = occ.plan_info();
    eprintln!(
        "[{T118}] run {:.1} s, span stats {:.1} s, {} rows, plan v{plan_version}, service {:?}",
        wall.elapsed().as_secs_f64(),
        t.elapsed().as_secs_f64(),
        rows.len(),
        occ.stats()
    );
    for r in &rows {
        r.validate().unwrap();
    }

    // ---- Truth, for assertions only. ----
    let truth = SceneTruth::load(&out).unwrap();
    let per = truth.schedule["stats"]["per_channel"].as_array().unwrap();
    let sampled = &truth.schedule["sampled_fco"]["by_channel"];
    let extent = |r: &hk_model::attention::occupancy::OccupancyStat| match r.subject {
        OccupancySubject::Channel { key } => Some(key.freq(f_cell)),
        OccupancySubject::Band { .. } => None,
    };
    eprintln!(
        "[{T118}] {:<8} {:>12} {:>8} {:>8} {:>8} | {:>8} {:>17} {:>5} {:>7} {:>6}",
        "kind",
        "center_MHz",
        "target",
        "realized",
        "sampled",
        "fco",
        "ci95",
        "n",
        "n_eff",
        "inside"
    );
    let (mut matched, mut inside, mut missing) = (0, 0, Vec::new());
    for c in &truth.channels {
        let p = per
            .iter()
            .find(|p| p["channel"].as_u64() == Some(c.channel))
            .unwrap();
        let realized = p["fco_realized"].as_f64().unwrap();
        let row = rows.iter().find(|r| {
            r.fco.is_some()
                && extent(r).is_some_and(|f| f.lo_hz <= c.center_hz && c.center_hz <= f.hi_hz)
        });
        let samp = sampled[c.channel.to_string()]["fco"]
            .as_f64()
            .unwrap_or(f64::NAN);
        let target = p["target_fco"].as_f64().unwrap_or(f64::NAN);
        match row {
            Some(r) => {
                let ci = r.confidence.unwrap();
                let ok = realized >= ci.lo - 1e-9 && realized <= ci.hi + 1e-9;
                matched += 1;
                inside += usize::from(ok);
                eprintln!(
                    "[{T118}] {:<8} {:>12.4} {target:>8.3} {realized:>8.4} {samp:>8.3} | {:>8.4} \
                     [{:>6.4},{:>6.4}] {:>5} {:>7.1} {ok:>6}",
                    c.kind,
                    c.center_hz / 1e6,
                    r.fco.unwrap(),
                    ci.lo,
                    ci.hi,
                    r.n_revisits,
                    ci.n_eff
                );
            }
            None => {
                eprintln!(
                    "[{T118}] {:<8} {:>12.4} {target:>8.3} {realized:>8.4} {samp:>8.3} | no learned channel",
                    c.kind,
                    c.center_hz / 1e6
                );
                if realized >= 0.05 {
                    missing.push(c.kind.clone());
                }
            }
        }
    }
    // 1.
    assert!(
        missing.is_empty(),
        "[{T118}] busy channels not learned: {missing:?}"
    );
    // 2.
    assert!(matched >= 5, "[{T118}] only {matched} channels matched");
    assert!(
        inside * 4 >= matched * 3,
        "[{T118}] realized FCO inside the 95 % interval for {inside}/{matched} channels"
    );
    // 3.
    for r in &rows {
        let Some(f) = extent(r) else { continue };
        let holds = truth.channels.iter().any(|c| {
            let h = c.bandwidth_hz / 2.0;
            c.center_hz + h > f.lo_hz && c.center_hz - h < f.hi_hz
        });
        if !holds {
            assert!(
                r.fco.unwrap_or(0.0) <= 0.05,
                "[{T118}] phantom channel {:.4}-{:.4} MHz fco {:?}",
                f.lo_hz / 1e6,
                f.hi_hz / 1e6,
                r.fco
            );
        }
    }
    // 4.
    let band_row = rows
        .iter()
        .find(|r| matches!(r.subject, OccupancySubject::Band { .. }))
        .expect("a band row");
    eprintln!(
        "[{T118}] band: fco {:?} fbo {:?} sro {:?} n {} threshold {:.1} dB clamped {}",
        band_row.fco,
        band_row.fbo,
        band_row.sro,
        band_row.n_revisits,
        band_row.threshold_db,
        band_row.guard_clamped
    );
    assert!(band_row.fbo.is_some() && band_row.sro.is_some());
    assert!(plan_version >= 1, "[{T118}] no channel plan learned");
    let stored = occ
        .query(&OccupancyQuery {
            freq: band,
            span,
            interval: SeriesInterval::Min15,
            subject: None,
            f_cell_hz: f_cell,
            limit: 100_000,
        })
        .unwrap();
    eprintln!("[{T118}] {} persisted 15-min rows", stored.rows.len());
    assert!(!stored.rows.is_empty(), "[{T118}] no 15-min rows persisted");
    assert!(stored.rows.iter().all(|r| {
        r.fco_window
            .is_some_and(|w| w.start <= r.interval.start && w.end >= r.interval.end)
    }));
}
