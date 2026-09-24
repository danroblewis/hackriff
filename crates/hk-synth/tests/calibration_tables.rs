//! The shipped calibration tables against fresh noise (ADR-0015 §2.2: "checked by a Rust test
//! that re-samples 1 000 noise windows"; §13.6: "the test must also assert that a table
//! **refuses** a level above its own `admissible_bits`"; T-853 = MAUTO M-2).
//!
//! For every block with a table, 1000 windows are drawn from its null chain with seeds disjoint
//! from the generator's, at the table's smaller support, and every published level is checked:
//! the share of fresh null windows at or beyond the level's threshold must match the level's
//! realised significance within a 5σ binomial band. A table that over-claimed — `eye_open`'s
//! 6 bits of claim for 2.41 bits of fact (docs/21 §5.2) — fails here by an order of magnitude.

use hk_blocks::Registry;
use hk_synth::calibration::{NullKind, Threshold, Unexpressible, support_matches};
use hk_synth::nullchain::{evidence_direction, null_chains};
use hk_synth::{CalibrationSet, Fill};

const WINDOWS: u64 = 1000;
/// Disjoint from `calibration_draws`' seeds.
const SEED_BASE: u64 = 1 << 40;

#[test]
fn every_table_holds_on_1000_fresh_null_windows_and_refuses_beyond_its_reach() {
    let registry = Registry::builtin();
    let set = CalibrationSet::builtin().expect("built-in tables load");
    let threads = 6u64;
    let mut checked = 0;
    for chain in null_chains() {
        let block = chain.block_version(&registry);
        let table = set
            .get(&block)
            .unwrap_or_else(|| panic!("{block}: every null chain ships a table"));
        let n = chain.supports[0];
        let per = WINDOWS.div_ceil(threads);
        let draws: Vec<Vec<hk_blocks::Evidence>> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..threads)
                .map(|t| {
                    let (chain, registry) = (&chain, &registry);
                    s.spawn(move || {
                        (t * per..((t + 1) * per).min(WINDOWS))
                            .map(|w| chain.draw(registry, n, SEED_BASE + w).expect("draw"))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(draws.len() as u64, WINDOWS);
        let metrics: Vec<_> = draws[0].iter().map(|e| e.metric).collect();
        for metric in metrics {
            let cell = table
                .cell_for(metric, NullKind::Noise, n)
                .unwrap_or_else(|| panic!("{block} {metric:?}: no cell at n = {n}"));
            let vals: Vec<f32> = draws
                .iter()
                .map(|d| {
                    let e = d
                        .iter()
                        .find(|e| e.metric == metric)
                        .expect("metric in every window");
                    assert!(
                        support_matches(cell.n, e.n),
                        "{block}: n {} vs cell {}",
                        e.n,
                        cell.n
                    );
                    evidence_direction(metric, e.raw)
                })
                .collect();
            for level in table.levels(&cell).expect("levels") {
                let k = vals.iter().filter(|&&v| v >= level.threshold).count() as f64;
                let p = (-f64::from(level.realised_bits)).exp2();
                let mean = WINDOWS as f64 * p;
                let band = 5.0 * (mean * (1.0 - p)).sqrt() + 2.0;
                assert!(
                    (k - mean).abs() <= band,
                    "{block} {metric:?} n={n}: level {} bits (realised {}) — {k} of {WINDOWS} fresh \
                     null windows beyond {}, expected {mean:.1} ± {band:.1}",
                    level.bits,
                    level.realised_bits,
                    level.threshold
                );
            }
            // The refusal (§13.6): nothing above admissible_bits is answered with a threshold.
            let adm = table.admissible_bits(&cell).unwrap();
            assert!(matches!(
                table.threshold(&cell, adm + 1.0),
                Threshold::Shortfall { .. }
            ));
            assert!(matches!(
                table.threshold(&cell, 8.0),
                Threshold::Shortfall {
                    reason: Unexpressible::AboveCalibratedClaimCap { .. },
                    ..
                }
            ));
            // And the scorer never credits more than the table can express.
            let top = vals.iter().copied().fold(f32::MIN, f32::max);
            let credited = table
                .score_raw(
                    metric,
                    if metric_smaller(metric) { -top } else { top },
                    n,
                    Fill::new(8.0, 0.0),
                )
                .credited_bits();
            assert!(credited <= adm, "{block} {metric:?}: {credited} > {adm}");
            checked += 1;
        }
    }
    assert!(
        checked >= 13,
        "every published calibrated metric was checked ({checked})"
    );
}

fn metric_smaller(m: hk_synth::MetricId) -> bool {
    hk_synth::nullchain::smaller_is_evidence(m)
}
