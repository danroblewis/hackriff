//! T-141: the floor of mixed-shape tiles from the Gamma mixture of their pooled level-0 values,
//! and format-3 tiles (no per-shape value counts) keeping their pre-T-141 behaviour.

use super::*;

const FLOOR_DB: f32 = -100.0;

/// Ten 100-ms frames per second over 16 one-bin cells: frames `i % 10 < 3` are 4-look, the rest
/// 40-look (`shapes` of `(k_a, k_b)`), each bin a mean-1 Gamma(k) draw at `FLOOR_DB`.
fn fold(p: &mut Pyramid, rng: &mut Rng, from_s: i64, to_s: i64, shapes: (u32, u32)) {
    let mut psd = vec![0f32; 16];
    for s in from_s..to_s {
        for i in 0..10i64 {
            let k = if i < 3 { shapes.0 } else { shapes.1 };
            for v in &mut psd {
                *v = lin(FLOOR_DB) * rng.gamma(k) as f32;
            }
            let mut f = frame(T0 + s * S + i * S / 10, S / 10, 0.0, 1000.0, &psd);
            f.noise_shape = NoiseShape::CellShape(k as f32);
            p.ingest(&f).unwrap();
        }
    }
}

fn floors(h: &RegionHistory) -> Vec<f32> {
    h.cells
        .iter()
        .filter(|c| c.observed())
        .map(|c| c.floor_db)
        .collect()
}

fn median(v: &[f32]) -> f32 {
    let mut v = v.to_vec();
    v.sort_by(f32::total_cmp);
    v[v.len() / 2]
}

/// Shapes 30 and 300 (cell shapes of real history rows are hundreds): the mixture-corrected
/// median floor is within 0.2 dB of the injected level.
#[test]
fn mixed_shape_floor_within_0_2_db_in_the_rank_rules_regime() {
    let dir = TempDir::new("mixture-valid");
    let mut p = Pyramid::open(&dir.0, cfg(vec![level(1, 60), level(2, 60)], 16)).unwrap();
    let mut rng = Rng(0x1410);
    fold(&mut p, &mut rng, 0, 120, (30, 300));
    p.seal_through(ts(T0 + 3600 * S)).unwrap();
    let h = query(
        &p,
        (0.0, 16_000.0),
        (T0, T0 + 120 * S),
        Resolution::Level(0),
    );
    assert!(h.provenance.cell_shape_mixture().is_some());
    let f = floors(&h);
    assert!(f.len() == 120 * 16 && f.iter().all(|v| v.is_finite()));
    let m = median(&f);
    let l1 = query(
        &p,
        (0.0, 16_000.0),
        (T0, T0 + 120 * S),
        Resolution::Level(1),
    );
    let f1 = floors(&l1);
    assert!(!f1.is_empty() && f1.iter().all(|v| v.is_finite()), "{f1:?}");
    let m1 = median(&f1);
    eprintln!(
        "T-141 mixture (30, 300): median floor level 0 {m:.3} dB, level 1 {m1:.3} dB (injected \
         {FLOOR_DB})"
    );
    assert!((m - FLOOR_DB).abs() <= 0.2, "mixture floor {m}");
    assert!((m1 - FLOOR_DB).abs() <= 0.2, "level-1 mixture floor {f1:?}");
}

#[test]
fn mixed_shape_tiles_correct_with_the_gamma_mixture_bias() {
    let dir = TempDir::new("mixture");
    let mut p = Pyramid::open(&dir.0, cfg(vec![level(1, 60), level(2, 60)], 16)).unwrap();
    let mut rng = Rng(0x141);
    // Two mixed level-0 tiles, then a uniform (40-look) one.
    fold(&mut p, &mut rng, 0, 120, (4, 40));
    fold(&mut p, &mut rng, 120, 180, (40, 40));
    p.seal_through(ts(T0 + 3600 * S)).unwrap();
    let span = (0.0, 16_000.0);
    let mixed = query(&p, span, (T0, T0 + 120 * S), Resolution::Level(0));
    let uniform = query(&p, span, (T0 + 120 * S, T0 + 180 * S), Resolution::Level(0));
    let prov = &mixed.provenance;
    assert!(
        prov.cell_shape_mixed && prov.other_shape_values == 0,
        "{prov:?}"
    );
    let values: Vec<(f32, u64)> = prov.cell_shapes.clone();
    assert_eq!(values, vec![(4.0, 3 * 120 * 16), (40.0, 7 * 120 * 16)]);

    let f_mixed = floors(&mixed);
    assert_eq!(f_mixed.len(), 120 * 16);
    assert!(
        f_mixed.iter().all(|v| v.is_finite()),
        "every mixed cell has a floor"
    );
    let m = median(&f_mixed);
    // What each single shape's correction would have read (10 frames per cell).
    let raw: Vec<f32> = mixed.cells.iter().map(|c| c.p_low_db).collect();
    let p10 = hk_dsp::radiometry::exact_percentile_probability(10.0, 10);
    let naive = |k: f64| median(&raw) - hk_dsp::radiometry::percentile_bias_db(k, p10) as f32;
    eprintln!(
        "T-141 mixture: level-0 median floor {m:.3} dB (injected {FLOOR_DB}); single-shape k 4 \
         {:.3}, k 40 {:.3}",
        naive(4.0),
        naive(40.0)
    );
    // A 4-look component at 10 frames is below the rank rule's validity (n_c ≳ 30, hk-dsp
    // `radiometry::bias`): its own single-shape correction errs by ≈ −0.27 dB there. The mixture
    // must still beat both single-shape corrections by far; its accuracy bound is asserted in the
    // valid regime (`mixed_shape_floor_within_0_2_db_in_the_rank_rules_regime`).
    assert!((naive(4.0) - FLOOR_DB).abs() > 0.3 && (naive(40.0) - FLOOR_DB).abs() > 0.3);
    assert!(
        (m - FLOOR_DB).abs()
            < 0.5
                * (naive(4.0) - FLOOR_DB)
                    .abs()
                    .min((naive(40.0) - FLOOR_DB).abs()),
        "mixture floor {m}"
    );

    // The rolled-up (histogram, p = q) cells of the mixed hour.
    let l1 = query(&p, span, (T0, T0 + 120 * S), Resolution::Level(1));
    let f1 = floors(&l1);
    assert!(!f1.is_empty() && f1.iter().all(|v| v.is_finite()), "{f1:?}");
    // Rolled up, p = q over the pooled histogram; the same out-of-regime 4-look component, so the
    // same relative criterion as level 0 (the 0.2-dB bound is asserted in the valid regime).
    let raw1: Vec<f32> = l1.cells.iter().map(|c| c.p_low_db).collect();
    let naive1 = |k: f64| median(&raw1) - hk_dsp::radiometry::percentile_bias_db(k, 0.1) as f32;
    let m1 = median(&f1);
    eprintln!(
        "T-141 mixture: level-1 median floor {m1:.3} dB; single-shape k 4 {:.3}, k 40 {:.3}",
        naive1(4.0),
        naive1(40.0)
    );
    assert!(
        (m1 - FLOOR_DB).abs()
            < 0.5
                * (naive1(4.0) - FLOOR_DB)
                    .abs()
                    .min((naive1(40.0) - FLOOR_DB).abs()),
        "level-1 floor {f1:?}"
    );

    // Format 3 recorded no per-shape counts: rewritten, the mixed tiles give no floor (as before
    // T-141) and the uniform tile keeps exactly its floor.
    let f_uniform = floors(&uniform);
    assert!(f_uniform.iter().all(|v| v.is_finite()));
    let hist = p.config().histogram;
    let g = p.geometry().levels[0];
    for key in p.sealed_keys(0) {
        let path = p.tile_path(key);
        let tile = codec::decode(&path, &g, usize::from(hist.bins))
            .unwrap()
            .unwrap();
        let (mut buf, mut payload) = (Vec::new(), Vec::new());
        codec::encode_format(
            3,
            &tile,
            true,
            PowerUnit::Dbfs,
            &g,
            &hist,
            (10.0, 90.0),
            Some(3),
            &mut buf,
            &mut payload,
        );
        assert_eq!(u16::from_le_bytes([buf[8], buf[9]]), 3);
        fs::write(&path, &buf).unwrap();
    }
    let old = Pyramid::open(&dir.0, cfg(vec![level(1, 60), level(2, 60)], 16)).unwrap();
    let mixed3 = query(&old, span, (T0, T0 + 120 * S), Resolution::Level(0));
    let uniform3 = query(
        &old,
        span,
        (T0 + 120 * S, T0 + 180 * S),
        Resolution::Level(0),
    );
    assert!(mixed3.provenance.cell_shape_mixture().is_none());
    assert!(floors(&mixed3).iter().all(|v| v.is_nan()));
    assert_eq!(
        mixed3.cells.iter().map(|c| c.p_low_db).collect::<Vec<_>>(),
        raw
    );
    assert_eq!(floors(&uniform3), f_uniform);
}
