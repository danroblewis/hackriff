//! SPACE-050 through the composed pipeline (T-027): the `injected_floor` scenario (six band
//! segments at 1 Msps, each a retune with a known floor) replayed once; the history reader's
//! `FloorProduct` answers floor-vs-time per segment within ±1 dB of the injected floor
//! (uncalibrated dBFS/Hz: no calibration is loaded).

mod common;

use common::*;
use hk_e2e::{Role, SynthRequest, synth_or_skip};
use hk_model::{FreqRange, PowerUnit};
use hk_store::Resolution;
use serde_json::json;

const SPACE_050: &str = "SPACE-050";

#[test]
fn space_050_injected_floor_recovered_from_the_pipeline_floor_product() {
    let out = synth_or_skip!(
        SynthRequest::new("injected_floor")
            .seed(4)
            .param("segment_duration_s", 1.0)
    );
    let fx = out.fixture(0).unwrap();
    let fs = fx.sample_rate;
    let dir = TempDir::new("space050");
    let (cfg, replay) = replay_config(&dir.0, &fx.meta_path, json!({}), hk_core::Pacing::Unpaced);
    let handle = start(cfg, replay);
    let product = handle.floor_product();
    let s = handle.wait().unwrap();
    eprintln!("{}", s.to_text());
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    assert_eq!(s.always_on_lost_samples, 0);
    assert!(s.counter("/history/frames_ingested") > 0);
    assert!(
        s.counter("/history/tiles_written") > 0,
        "[{SPACE_050}] tiles"
    );

    let floors = fx.with_role(Role::Floor);
    assert_eq!(floors.len(), 6);
    let p = product.lock().unwrap();
    let mut worst = 0.0f64;
    for (i, seg) in floors.iter().enumerate() {
        let cap = fx.capture_at(seg.sample_start).unwrap();
        let fc = cap.frequency.unwrap();
        let t0 =
            hk_core::source::sigmf_replay::parse_sigmf_datetime(cap.datetime.as_deref().unwrap())
                .unwrap();
        let t1 = t0.saturating_add_nanos((seg.sample_count as f64 * 1e9 / fs) as i64);
        let want = seg.expect_f64("expected_floor_dbfs") - 10.0 * seg.bandwidth_hz().log10();
        let fvt = p
            .floor_vs_time(
                FreqRange::centered(fc, 0.8 * fs),
                t0,
                t1,
                Resolution::Level(0),
            )
            .unwrap();
        let observed: Vec<_> = fvt.steps.iter().filter(|s| !s.is_gap()).collect();
        assert!(
            !observed.is_empty(),
            "[{SPACE_050}] segment {i}: no observed step"
        );
        for st in observed {
            assert_eq!(st.unit, Some(PowerUnit::Dbfs), "uncalibrated");
            let got = st.value_db_per_hz.expect("floor value");
            eprintln!(
                "[{SPACE_050}] segment {i} ({:.2} MHz): {got:.2} dBFS/Hz vs injected {want:.2} \
                 (err {:+.2}, σ {:.2}, {} cells)",
                fc / 1e6,
                got - want,
                st.uncertainty_db,
                st.cells
            );
            worst = worst.max((got - want).abs());
            assert!(
                (got - want).abs() <= 1.0,
                "[{SPACE_050}] segment {i}: {got:.2} vs {want:.2} dBFS/Hz"
            );
        }
    }
    eprintln!("[{SPACE_050}] worst floor error {worst:.2} dB");
}
