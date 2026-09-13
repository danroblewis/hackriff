//! AWARE-042 through the composed pipeline (T-027): a rendered `occupancy_multi_hour` window
//! (8 NBFM channels at 12.5 kHz spacing, 200 kS/s) replayed once. Every channel with a burst of
//! at least 0.3 s in the window gets detections and a track that reaches the inventory within the
//! channel, and the history pyramid answers a region-over-time query over the channels.

mod common;

use common::*;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::{FreqRange, InventoryQuery, Region, TimeRange, Timestamp};
use hk_store::{RegionQuery, Resolution};
use serde_json::json;

const AWARE_042: &str = "AWARE-042";

#[test]
fn aware_042_occupancy_window_tracks_per_channel_and_history_query() {
    let out = synth_or_skip!(
        SynthRequest::new("occupancy_multi_hour")
            .seed(11)
            .param("windows", 1)
            .param("window_duration_s", 2.0)
    );
    let fx = out.fixture(0).unwrap();
    let fs = fx.sample_rate;
    let dir = TempDir::new("aware042");
    let (cfg, replay) = replay_config(&dir.0, &fx.meta_path, json!({}), hk_core::Pacing::Unpaced);
    let handle = start(cfg, replay);
    let product = handle.floor_product();
    let s = handle.wait().unwrap();
    eprintln!("{}", s.to_text());
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    assert_eq!(s.always_on_lost_samples, 0);

    let bursts = fx.of_kind("nbfm-burst");
    let mut channels: Vec<(f64, f64)> = Vec::new();
    for b in &bursts {
        let dur = b.sample_count as f64 / fs;
        if dur >= 0.3 && !channels.iter().any(|c| (c.0 - b.center_hz()).abs() < 1.0) {
            channels.push((b.center_hz(), dur));
        }
    }
    assert!(
        !channels.is_empty(),
        "[{AWARE_042}] no long bursts in the window"
    );
    let repo = repo(&dir.0);
    let all = Region::new(
        FreqRange::new(0.0, 1e12),
        TimeRange::new(
            Timestamp::from_unix_nanos(0),
            Timestamp::from_unix_nanos(i64::MAX / 2),
        ),
    );
    let _ = all;
    for &(fc, dur) in &channels {
        let band = FreqRange::centered(fc, 12_500.0);
        let dets = repo
            .detections_in_region(&Region::new(
                band,
                TimeRange::new(
                    Timestamp::from_unix_nanos(0),
                    Timestamp::from_unix_nanos(i64::MAX / 2),
                ),
            ))
            .unwrap();
        let emitters = inventory(
            &repo,
            InventoryQuery {
                freq: Some(FreqRange::centered(fc, 6_000.0)),
                ..InventoryQuery::default()
            },
        );
        eprintln!(
            "[{AWARE_042}] channel {:.4} MHz ({dur:.2} s on): {} detections, {} inventory emitters",
            fc / 1e6,
            dets.len(),
            emitters.len()
        );
        assert!(!dets.is_empty(), "[{AWARE_042}] channel {fc} not detected");
        assert!(
            !emitters.is_empty(),
            "[{AWARE_042}] channel {fc}: no track reached the inventory"
        );
    }
    assert!(
        s.counter("/detect/track_rows") >= channels.len() as u64,
        "[{AWARE_042}] tracks {} for {} channels",
        s.counter("/detect/track_rows"),
        channels.len()
    );

    // History: the region-over-time query over the channel raster is observed.
    let p = product.lock().unwrap();
    let t0 = hk_core::source::sigmf_replay::parse_sigmf_datetime(
        fx.meta.captures[0].datetime.as_deref().unwrap(),
    )
    .unwrap();
    let h = p
        .uncalibrated_pyramid()
        .query(&RegionQuery {
            freq: FreqRange::new(446.0e6, 446.1e6),
            time: TimeRange::new(t0, t0.saturating_add_nanos(2_000_000_000)),
            resolution: Resolution::Level(0),
        })
        .unwrap();
    let observed = h.cells.iter().filter(|c| c.observed()).count();
    eprintln!(
        "[{AWARE_042}] history L{} {}x{} cells, {observed} observed, {} tiles written",
        h.level,
        h.nt,
        h.nf,
        s.counter("/history/tiles_written")
    );
    assert!(
        observed > 0,
        "[{AWARE_042}] history query has no observed cells"
    );
    assert!(s.counter("/history/tiles_written") > 0);
}
