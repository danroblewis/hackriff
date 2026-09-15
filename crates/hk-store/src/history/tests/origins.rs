//! T-133: tiles record source and site; filtered queries; format 1/2 tiles read as unknown origin.

use hk_model::TileKey;
use hk_model::attention::baseline::SiteKey;
use hk_model::ids::SiteId;

use super::*;

const F_LO: f64 = 16_000.0;

/// 40 s at one frame per second on L0 block 1 (16–32 kHz), levels 1 s × 1 kHz (10-s tiles) and
/// 10 s × 2 kHz (40-s tiles):
/// - tile 0 (0–10 s): site A, source 1, −100 dB;
/// - tile 1 (10–20 s): site A, source 2, −100 dB;
/// - tile 2 (20–30 s): mobile, source 1, −80 dB;
/// - tile 3 (30–40 s): site A until 35 s, then mobile (mixed), source 1.
fn scene(dir: &TempDir, a: SiteKey) -> Pyramid {
    let mut p = Pyramid::open(&dir.0, cfg(vec![level(1, 10), level(2, 4)], 16)).unwrap();
    for s in 0..40i64 {
        let (site, source) = match s {
            0..10 => (a, 1),
            10..20 => (a, 2),
            20..30 => (SiteKey::Mobile, 1),
            30..35 => (a, 1),
            _ => (SiteKey::Mobile, 1),
        };
        let db = if site == SiteKey::Mobile {
            -80.0
        } else {
            -100.0
        };
        let psd = vec![lin(db); 16];
        let mut f = frame(T0 + s * S, S, F_LO, 1000.0, &psd);
        f.source = source;
        f.site = Some(site);
        assert_eq!(p.ingest(&f).unwrap(), IngestOutcome::Folded);
    }
    p.seal_through(ts(T0 + 80 * S)).unwrap();
    p
}

fn filtered(p: &Pyramid, level: u8, filter: OriginFilter) -> RegionHistory {
    p.query_filtered(
        &RegionQuery {
            freq: FreqRange::new(F_LO, 2.0 * F_LO),
            time: TimeRange::new(ts(T0), ts(T0 + 40 * S)),
            resolution: Resolution::Level(level),
        },
        &filter,
    )
    .unwrap()
}

fn site(s: SiteKey) -> OriginFilter {
    OriginFilter {
        site: OriginField::Is(s),
        ..OriginFilter::ANY
    }
}

/// Rows with every cell observed / with none observed; panics on a partly observed row.
fn observed_rows(h: &RegionHistory) -> Vec<bool> {
    (0..h.nt)
        .map(|t| {
            let n = h.row(t).iter().filter(|c| c.observed()).count();
            assert!(n == 0 || n == h.nf, "row {t}: {n} of {} observed", h.nf);
            n == h.nf
        })
        .collect()
}

#[test]
fn level0_filters_by_site_and_source_and_counts_exclusions() {
    let dir = TempDir::new("origin-l0");
    let a = SiteKey::Site(SiteId::new());
    drop(scene(&dir, a));
    // Reopened: every tile is read back from disk (format 3).
    let p = Pyramid::open(&dir.0, cfg(vec![level(1, 10), level(2, 4)], 16)).unwrap();

    let all = filtered(&p, 0, OriginFilter::ANY);
    assert_eq!(all.filter, None, "an unfiltered query reports no filter");
    assert!(observed_rows(&all).iter().all(|&o| o));
    let origins: HashMap<Origin, u64> = all.provenance.origins.iter().copied().collect();
    let o = |source, site| Origin {
        source: Some(source),
        site: Some(site),
    };
    assert_eq!(origins[&o(1, a)], 15, "{origins:?}");
    assert_eq!(origins[&o(2, a)], 10);
    assert_eq!(origins[&o(1, SiteKey::Mobile)], 15);

    let h = filtered(&p, 0, site(a));
    let rows = observed_rows(&h);
    assert!(rows[..20].iter().all(|&o| o) && rows[20..].iter().all(|&o| !o));
    assert!(h.cells[..20 * 16].iter().all(|c| c.mean_db == -100.0));
    let f = h.filter.unwrap();
    assert_eq!(
        (f.tiles_matched, f.tiles_mixed, f.tiles_other),
        (2, 1, 1),
        "{f:?}"
    );
    assert_eq!((f.cells_excluded, f.cells_from_children), (20 * 16, 0));
    assert!(h.provenance.origins.iter().all(|(o, _)| o.site == Some(a)));
    assert_eq!(h.provenance.frames, 20);

    let h = filtered(&p, 0, site(SiteKey::Mobile));
    let rows = observed_rows(&h);
    assert!(
        rows.iter()
            .enumerate()
            .all(|(t, &o)| o == (20..30).contains(&t))
    );
    assert!(h.cells[20 * 16..30 * 16].iter().all(|c| c.mean_db == -80.0));

    let src_and_site = OriginFilter {
        source: OriginField::Is(2),
        site: OriginField::Is(a),
    };
    let rows = observed_rows(&filtered(&p, 0, src_and_site));
    assert!(
        rows.iter()
            .enumerate()
            .all(|(t, &o)| o == (10..20).contains(&t))
    );

    // Every frame here has a known source and site: `unknown` matches nothing.
    for unknown in [
        OriginFilter {
            source: OriginField::Unknown,
            ..OriginFilter::ANY
        },
        OriginFilter {
            site: OriginField::Unknown,
            ..OriginFilter::ANY
        },
    ] {
        let h = filtered(&p, 0, unknown);
        assert!(h.cells.iter().all(|c| !c.observed()));
        assert_eq!(h.filter.unwrap().tiles_other, 4);
    }
    // An unassigned site never contributed.
    let h = filtered(&p, 0, site(SiteKey::Unassigned));
    assert!(h.cells.iter().all(|c| !c.observed()));
}

#[test]
fn coarse_mixed_tile_keeps_cells_whose_finer_tile_passes_whole() {
    let dir = TempDir::new("origin-l1");
    let a = SiteKey::Site(SiteId::new());
    let p = scene(&dir, a);
    let all = filtered(&p, 1, OriginFilter::ANY);
    assert_eq!((all.nt, all.nf), (4, 8));

    // The one 40-s level-1 tile is mixed; its rows are the four 10-s level-0 tiles.
    let h = filtered(&p, 1, site(a));
    assert_eq!(observed_rows(&h), vec![true, true, false, false]);
    for i in 0..2 * 8 {
        assert_eq!(
            (h.cells[i].mean_db, h.cells[i].frames),
            (all.cells[i].mean_db, all.cells[i].frames),
            "kept cells are the stored cells"
        );
    }
    let f = h.filter.unwrap();
    assert_eq!((f.tiles_matched, f.tiles_mixed, f.tiles_other), (0, 1, 0));
    assert_eq!((f.cells_from_children, f.cells_excluded), (16, 16));
    // Provenance of the finer tiles actually returned, not of the mixed tile.
    assert_eq!(h.provenance.frames, 20);
    assert!(h.provenance.origins.iter().all(|(o, _)| o.site == Some(a)));

    let h = filtered(&p, 1, site(SiteKey::Mobile));
    assert_eq!(observed_rows(&h), vec![false, false, true, false]);
    let h = filtered(
        &p,
        1,
        OriginFilter {
            source: OriginField::Is(1),
            site: OriginField::Is(a),
        },
    );
    assert_eq!(observed_rows(&h), vec![true, false, false, false]);

    // A finer tile that is gone (evicted) cannot vouch for its coarse cell.
    let child = p.tile_path(TileKey {
        scheme: 7,
        level: 0,
        f_block: 1,
        t_block: T0 / (10 * S),
    });
    drop(p);
    fs::remove_file(child).unwrap();
    let p = Pyramid::open(&dir.0, cfg(vec![level(1, 10), level(2, 4)], 16)).unwrap();
    assert_eq!(
        observed_rows(&filtered(&p, 1, site(a))),
        vec![false, true, false, false]
    );
}

#[test]
fn format_1_and_2_tiles_stay_readable_as_unknown_origin() {
    let dir = TempDir::new("origin-old");
    let a = SiteKey::Site(SiteId::new());
    let p = scene(&dir, a);
    let before = filtered(&p, 0, OriginFilter::ANY);
    let before_l1 = filtered(&p, 1, OriginFilter::ANY);
    // Level-0 tiles 0–1 as format 1, the rest (and the level-1 tile) as format 2.
    let hist = p.config().histogram;
    for level in 0..2 {
        let g = p.geometry().levels[level];
        for (i, key) in p.sealed_keys(level).into_iter().enumerate() {
            let path = p.tile_path(key);
            let tile = codec::decode(&path, &g, usize::from(hist.bins))
                .unwrap()
                .unwrap();
            assert!(!tile.prov.origins.is_empty());
            let mut buf = Vec::new();
            let format = if level == 0 && i < 2 { 1 } else { 2 };
            if format == 1 {
                codec::encode_v1(
                    &tile,
                    true,
                    PowerUnit::Dbfs,
                    &g,
                    &hist,
                    (10.0, 90.0),
                    &mut buf,
                );
            } else {
                let mut payload = Vec::new();
                codec::encode_format(
                    2,
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
            }
            assert_eq!(u16::from_le_bytes([buf[8], buf[9]]), format);
            let back = codec::decode_bytes(&buf, &g, usize::from(hist.bins)).unwrap();
            assert_eq!(back.prov.origins, vec![(Origin::UNKNOWN, tile.prov.frames)]);
            fs::write(&path, &buf).unwrap();
        }
    }
    drop(p);
    let p = Pyramid::open(&dir.0, cfg(vec![level(1, 10), level(2, 4)], 16)).unwrap();
    assert_eq!(p.stats().files_ignored, 0);

    let stats = |h: &RegionHistory| -> Vec<(u32, u32)> {
        h.cells
            .iter()
            .map(|c| (c.frames, c.mean_db.to_bits()))
            .collect()
    };
    let after = filtered(&p, 0, OriginFilter::ANY);
    assert_eq!(
        stats(&after),
        stats(&before),
        "unfiltered reads are unchanged"
    );
    assert_eq!(after.provenance.origins, vec![(Origin::UNKNOWN, 40)]);
    assert_eq!(
        stats(&filtered(&p, 1, OriginFilter::ANY)),
        stats(&before_l1)
    );

    // A known site or source matches no old frame; `unknown` matches all of them.
    for (level, known) in [(0, site(a)), (1, site(SiteKey::Mobile))] {
        let h = filtered(&p, level, known);
        assert!(h.cells.iter().all(|c| !c.observed()), "L{level}");
    }
    let h = filtered(
        &p,
        0,
        OriginFilter {
            source: OriginField::Is(1),
            ..OriginFilter::ANY
        },
    );
    assert!(h.cells.iter().all(|c| !c.observed()));
    let unknown = OriginFilter {
        source: OriginField::Unknown,
        site: OriginField::Unknown,
    };
    let h = filtered(&p, 0, unknown);
    assert_eq!(stats(&h), stats(&before));
    assert_eq!(h.filter.unwrap().tiles_matched, 4);
    assert_eq!(stats(&filtered(&p, 1, unknown)), stats(&before_l1));
}

#[test]
fn origins_beyond_the_cap_read_as_unknown() {
    let mut sum = ProvenanceSummary::default();
    let sites: Vec<SiteKey> = (0..MAX_ORIGINS + 1)
        .map(|_| SiteKey::Site(SiteId::new()))
        .collect();
    for &s in &sites {
        sum.merge(&ProvenanceSummary {
            frames: 1,
            origins: vec![(
                Origin {
                    source: Some(0),
                    site: Some(s),
                },
                1,
            )],
            ..ProvenanceSummary::default()
        });
    }
    assert_eq!(sum.origins.len(), MAX_ORIGINS);
    assert_eq!(sum.other_origin_frames, 1);
    let last = site(*sites.last().unwrap());
    assert_eq!(sum.origin_frames(&last), (0, MAX_ORIGINS as u64 + 1));
    assert_eq!(sum.origin_match(&last), OriginMatch::None);
    let unknown = OriginFilter {
        site: OriginField::Unknown,
        ..OriginFilter::ANY
    };
    assert_eq!(sum.origin_match(&unknown), OriginMatch::Mixed);
    assert_eq!(sum.origin_match(&site(sites[0])), OriginMatch::Mixed);
    assert_eq!(sum.origin_match(&OriginFilter::ANY), OriginMatch::All);
}
