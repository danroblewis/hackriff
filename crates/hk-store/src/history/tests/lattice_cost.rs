//! T-434: what the de-welded lattice costs on disk, **measured** through the production codec.
//!
//! `docs/16` §7 step 4 says this is where the storage cost lands, and §5.2 accepted that cost
//! sight-unseen. With independent axes the level *count* grows multiplicatively — an 8 × 8 lattice
//! is 64 planes where a ladder had 5 — so the question this file answers is not *is it more* but
//! *how much more, and which retention bound binds where*.
//!
//! Everything here is a real `Pyramid` fed real frames, sealed through the real seal path, and
//! measured as **bytes on disk** (zstd payloads and all). Nothing is estimated from a cell count.
//! T-406 set the precedent: it encoded a scan record through the production codec, measured
//! 618 B/line, and found the thing the design doc had not stated — which bound binds depends on
//! the policy.

use super::*;

/// A noise floor with a few carriers on it: realistic input, because a flat spectrum compresses
/// to almost nothing and would flatter every number in this file.
fn spectrum(rng: &mut Rng, n: usize, t: i64) -> Vec<f32> {
    (0..n)
        .map(|b| {
            let floor = -100.0 + 10.0 * rng.gamma(4).log10() as f32;
            // Three carriers, one of them bursty, so occupancy and max-hold both have work to do.
            let carrier = if b % 97 == 11 {
                45.0
            } else if b % 211 == 60 {
                30.0
            } else if b % 53 == 7 && (t / 8) % 3 == 0 {
                25.0
            } else {
                0.0
            };
            lin(floor + carrier)
        })
        .collect()
}

/// Bytes of every sealed tile of `level` on disk, by reading the files back.
fn level_files(root: &std::path::Path, level: usize) -> (u64, u64) {
    let dir = root.join(format!("L{level}"));
    let (mut n, mut bytes) = (0u64, 0u64);
    let Ok(fs) = std::fs::read_dir(&dir) else {
        return (0, 0);
    };
    for f in fs.flatten() {
        let Ok(ts) = std::fs::read_dir(f.path()) else {
            continue;
        };
        for t in ts.flatten() {
            if t.path().extension().is_some_and(|e| e == "tile") {
                n += 1;
                bytes += t.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    (n, bytes)
}

/// **The measurement.** A 4 × 4 lattice with the shipped shape — uniform tiles, ×2 per step on
/// each axis independently — fed a fully-covered block of spectrum for long enough to seal every
/// node, then weighed.
///
/// The numbers this prints are the input to §7 step 5's addressing decision, so the test asserts
/// the *conclusions* rather than the byte counts: the counts move with zstd and with the data, the
/// shape of the cost does not.
#[test]
fn a_lattice_costs_about_four_times_its_finest_level_not_sixteen() {
    let dir = TempDir::new("lat-cost");
    let sh = ViewLattice {
        scheme: 12,
        f_cell_hz: 100_000.0,
        t_cell: Duration::from_secs(1),
        cells_per_block: 64,
        f_levels: 4,
        t_levels: 4,
    };
    let cfg = PyramidConfig {
        seal_lag: Duration::ZERO,
        checkpoint_interval: None,
        byte_budget: u64::MAX,
        ..PyramidConfig::view_lattice(sh)
    };
    // Cover the whole extent of the coarsest node twice over, so every level seals real tiles:
    // 8 frequency blocks (51.2 MHz, one coarsest tile wide) × 1024 s (two coarsest tiles tall).
    let n_bins = 64 * 8;
    let secs = 1024;
    let mut p = Pyramid::open(&dir.0, cfg).unwrap();
    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    for s in 0..secs {
        let psd = spectrum(&mut rng, n_bins, s);
        p.ingest(&frame(T0 + s * S, S, 0.0, 100_000.0, &psd))
            .unwrap();
    }
    p.seal_through(ts(T0 + secs * S)).unwrap();
    let root = dir.0.join("history").join(format!("s{}", sh.scheme));

    // **T-453, and it is the whole point of the ticket: none of that cost has been paid yet.**
    //
    // Everything below weighs the lattice a *viewer of every node* produces. `docs/16` §5.2 decided
    // the coarse nodes are built on demand, so after a full run with nobody watching, capture has
    // written node (0, 0) and nothing else — which is what makes the 4× of §6.4a a cost of
    // *looking* rather than a cost of *capturing*, and the lattice's node count a reach decision
    // rather than a gate-time one.
    let unread: u64 = (0..sh.f_levels * sh.t_levels)
        .map(|l| level_files(&root, l).0)
        .sum();
    let finest_tiles = level_files(&root, sh.index(0, 0)).0;
    println!(
        "\n  T-453: after the whole run, unread, the lattice holds {unread} tiles — \
         {finest_tiles} of them node (0, 0)'s"
    );
    assert!(finest_tiles > 0, "capture must still write its own product");
    assert_eq!(
        unread, finest_tiles,
        "a coarse node nobody has asked for must not exist on disk (docs/16 §5.2)"
    );

    // Now ask for every node, which is what the rest of this test is measuring the cost of.
    let whole = FreqRange::new(0.0, n_bins as f64 * 100_000.0);
    let span = TimeRange::new(ts(T0), ts(T0 + secs * S));
    for l in 0..sh.f_levels * sh.t_levels {
        p.materialize(l, whole, span).unwrap();
    }

    let mut per_node = vec![(0u64, 0u64); sh.f_levels * sh.t_levels];
    println!("\n  level_f  level_t   f cell    t cell     tiles      bytes   B/tile  B/cell");
    let mut total = 0u64;
    for i in 0..sh.f_levels {
        for j in 0..sh.t_levels {
            let l = sh.index(i, j);
            let (n, bytes) = level_files(&root, l);
            per_node[l] = (n, bytes);
            total += bytes;
            // Observed cells this node holds: the covered extent divided by its own cell.
            let cells = (n_bins as u64 / (1 << i)) * (secs as u64 / (1 << j));
            println!(
                "  {i:7}  {j:7}  {:7.0}k  {:6}s  {n:8}  {bytes:9}  {:7}  {:6.2}",
                sh.f_cell_hz * f64::from(1u32 << i) / 1000.0,
                1u64 << j,
                if n > 0 { bytes / n } else { 0 },
                if cells > 0 {
                    bytes as f64 / cells as f64
                } else {
                    0.0
                },
            );
        }
    }
    let finest = per_node[sh.index(0, 0)].1;
    // The **ladder embedded in the lattice**: the diagonal nodes are exactly the levels a welded
    // scheme would have built, folded ×2 on both axes at once. Comparing against it needs no
    // model — it is the same data, folded by the same code, in the same run.
    let ladder: u64 = (0..sh.f_levels.min(sh.t_levels))
        .map(|d| per_node[sh.index(d, d)].1)
        .sum();
    println!(
        "\n  finest node {finest} B · embedded ladder (the diagonal) {ladder} B · \
         whole lattice {total} B\n  lattice / finest {:.2}, lattice / ladder {:.2}\n",
        total as f64 / finest as f64,
        total as f64 / ladder as f64
    );

    // Every node sealed at least one tile: the lattice is produced by the one seal path, with no
    // second producer and no node left empty.
    for (l, node) in per_node.iter().enumerate() {
        let (i, j) = sh.coords(l);
        assert!(node.0 > 0, "node ({i},{j}) sealed no tile");
    }

    // **Finding 1: the level count is multiplicative, the bytes are not.** A ladder's levels
    // shrink ×4 a step (both axes at once) and sum to ≈1.33 × its finest. A lattice's shrink ×2 a
    // step on one axis and sum to (Σ2⁻ⁱ)(Σ2⁻ʲ) → 4. So 16 nodes cost a handful of finest levels,
    // not 16 of them, and an 8 × 8 lattice — 64 nodes — converges to about the same number.
    let vs_finest = total as f64 / finest as f64;
    let vs_ladder = total as f64 / ladder as f64;
    assert!(
        (3.0..7.0).contains(&vs_finest),
        "the lattice should cost a handful of finest nodes, not one per node: {vs_finest}"
    );
    assert!(
        (2.5..4.5).contains(&vs_ladder),
        "and about 3× the welded ladder over the same data: {vs_ladder}"
    );

    // **Finding 2, and it is the one §6 did not anticipate: a coarse cell costs MORE than a fine
    // one.** The naive model says bytes ∝ cells, so folding ×2 should halve a node. It does not
    // quite: a level-0 tile's neighbouring cells are a slowly-varying noise floor sampled a second
    // apart and zstd eats them, while a folded cell is a max over children and its neighbours are
    // much less alike. Every coarse node here lands near 3.4–3.6 B/cell against level 0's 2.3 —
    // roughly **half as much again per cell** — so the lattice's cost is the converging series
    // times that penalty, and pricing it as bytes-per-cell × cells alone understates it.
    let cells_of = |i: usize, j: usize| (n_bins as u64 >> i) * (secs as u64 >> j);
    let b_per_cell = |i: usize, j: usize| per_node[sh.index(i, j)].1 as f64 / cells_of(i, j) as f64;
    let fine = b_per_cell(0, 0);
    let coarse: f64 = (0..sh.f_levels)
        .flat_map(|i| (0..sh.t_levels).map(move |j| (i, j)))
        .filter(|&(i, j)| i + j > 0)
        .map(|(i, j)| b_per_cell(i, j))
        .sum::<f64>()
        / (sh.f_levels * sh.t_levels - 1) as f64;
    println!(
        "  B/cell: finest {fine:.2}, coarse mean {coarse:.2}, penalty {:.2}×",
        coarse / fine
    );
    assert!(
        coarse > fine,
        "folding destroys the correlation zstd was exploiting, so a coarse cell is not cheaper: \
         {coarse} vs {fine}"
    );

    // **Finding 3: neither axis is the cheap one.** A pure frequency fold halves the cells and so
    // does a pure time fold, so the two arms of the lattice cost the same. There is no axis to
    // materialise preferentially and no axis to leave to read-time folding on cost grounds.
    let f_arm: u64 = (0..sh.f_levels).map(|i| per_node[sh.index(i, 0)].1).sum();
    let t_arm: u64 = (0..sh.t_levels).map(|j| per_node[sh.index(0, j)].1).sum();
    let skew = f_arm as f64 / t_arm as f64;
    println!("  frequency arm {f_arm} B vs time arm {t_arm} B, skew {skew:.2}\n");
    assert!(
        (0.5..2.0).contains(&skew),
        "neither axis should dominate the other: {skew}"
    );
}

/// **Bytes per cell and the fixed cost of a tile, at the shipped 256 × 256 tile size.**
///
/// §5.5 requires uniform tiles so a client's tile *count* budget and its *byte* budget are the
/// same statement. That only holds if a tile's cost is dominated by its cells rather than its
/// header — so measure both, at the size `docs/16` §6.2 proposes, and at the fill fractions a
/// real survey produces (a tuned window covers part of a 25.6 MHz block, not all of it).
#[test]
fn a_tile_costs_its_cells_and_its_header_is_noise() {
    let sh = ViewLattice {
        scheme: 13,
        cells_per_block: 256,
        f_cell_hz: 100_000.0,
        t_cell: Duration::from_secs(1),
        f_levels: 2,
        t_levels: 2,
    };
    println!("\n  fill   observed cells      bytes   B/cell");
    let mut points = Vec::new();
    for fill_num in [1usize, 2, 4, 8, 16] {
        let dir = TempDir::new(&format!("lat-fill-{fill_num}"));
        let cfg = PyramidConfig {
            seal_lag: Duration::ZERO,
            checkpoint_interval: None,
            byte_budget: u64::MAX,
            ..PyramidConfig::view_lattice(sh)
        };
        let mut p = Pyramid::open(&dir.0, cfg).unwrap();
        let mut rng = Rng(0x5EED_0000_0000_0007);
        // `fill_num`/16 of the block's 256 frequency cells, for the whole 256 s tile.
        let bins = 256 * fill_num / 16;
        for s in 0..256 {
            let psd = spectrum(&mut rng, bins, s);
            p.ingest(&frame(T0 + s * S, S, 0.0, 100_000.0, &psd))
                .unwrap();
        }
        p.seal_through(ts(T0 + 256 * S)).unwrap();
        let root = dir.0.join("history").join(format!("s{}", sh.scheme));
        let (n, bytes) = level_files(&root, 0);
        assert_eq!(n, 1, "one level-0 tile");
        let cells = (bins * 256) as u64;
        println!(
            "  {:2}/16  {cells:14}  {bytes:9}  {:7.2}",
            fill_num,
            bytes as f64 / cells as f64
        );
        points.push((cells as f64, bytes as f64));
    }
    // Least squares over the five fills: bytes = fixed + per_cell × cells.
    let n = points.len() as f64;
    let (sx, sy): (f64, f64) = points
        .iter()
        .fold((0.0, 0.0), |a, p| (a.0 + p.0, a.1 + p.1));
    let (sxx, sxy): (f64, f64) = points
        .iter()
        .fold((0.0, 0.0), |a, p| (a.0 + p.0 * p.0, a.1 + p.0 * p.1));
    let per_cell = (n * sxy - sx * sy) / (n * sxx - sx * sx);
    let fixed = (sy - per_cell * sx) / n;
    println!("\n  bytes = {fixed:.0} + {per_cell:.2} × cells\n");

    // A cell costs a few bytes: the wire keeps seven quantised statistics and a varint frame
    // count, and zstd takes most of it back on a spectrum that is mostly noise floor.
    assert!(
        (0.5..14.0).contains(&per_cell),
        "per-cell cost outside the encoded width of a cell: {per_cell}"
    );
    // The fixed part is a header, a bitmap and a histogram section — small enough that a tile
    // count is an honest proxy for bytes, which is what §5.5's budget rests on.
    let full = fixed + per_cell * 65536.0;
    assert!(
        fixed < full * 0.2,
        "fixed cost {fixed} is too large a share of a full tile {full} for a count-based budget"
    );

    // And the tile **size** is not free either: the same data in 64 × 64 tiles cost 2.30 B/cell in
    // the lattice test above against ~1.3 here, because a bigger tile amortises its header and
    // gives zstd more to work with. §6.2's 256 × 256 is not only §5.5's uniformity argument; it is
    // also the cheaper tile.
    assert!(
        per_cell < 2.0,
        "a 256 × 256 tile should beat the 64 × 64 figure of ~2.3 B/cell: {per_cell}"
    );
}

/// **Which retention horizon binds, at which `(level_f, level_t)`** — the question §7 step 4 asks
/// and §5.4 could only answer for the welded scheme.
///
/// The arithmetic is derived from the measured per-cell cost above, exactly as T-406 derived both
/// of the observation log's bounds from one measured 618 B/line. The policy is stated, because
/// T-406's finding was that **which bound binds depends on the policy**: here, one front end
/// dwelling continuously on a 20 MHz window, which is the HackRF's practical live extent.
#[test]
fn the_view_lattice_is_so_cheap_that_the_record_horizon_binds_not_the_byte_budget() {
    // Measured above, at the shipped 256 × 256 tile size: bytes ≈ 1548 + 1.24 × cells for a
    // level-0 tile, and a coarse cell costs about 1.5× a level-0 one.
    const B_PER_CELL: f64 = 1.24;
    const COARSE_PENALTY: f64 = 1.5;
    // Measured above: the whole lattice against its finest node. An 8 × 8 lattice converges a
    // little above the 4 × 4 figure; 5.0 is the honest round number.
    const LATTICE_VS_FINEST: f64 = 5.0;
    // The welded ladder over the same data, from the same run.
    const LADDER_VS_FINEST: f64 = 1.5;

    let day = 86_400.0;
    let dwell_hz = 20e6;
    let budget = (8u64 << 30) as f64;
    let record_age_days = 180.0; // hk_store::observation::DEFAULT_MAX_AGE_NS, T-406.

    let per_day = |f_cell_hz: f64, t_cell_s: f64, spread: f64| {
        let cells = (dwell_hz / f_cell_hz) * (day / t_cell_s);
        cells * B_PER_CELL * spread
    };
    // The view lattice of `docs/16` §6.2: 100 kHz × 128 s at node (0, 0).
    let view = per_day(100_000.0, 128.0, LATTICE_VS_FINEST * COARSE_PENALTY);
    // Scheme 1, for contrast: 6.25 kHz × 1 s at level 0.
    let scheme1 = per_day(6250.0, 1.0, LADDER_VS_FINEST * COARSE_PENALTY);
    let view_days = budget / view;
    let scheme1_days = budget / scheme1;
    println!(
        "\n  20 MHz dwelled continuously, 8 GiB budget, 180-day record age:\n\
         \x20   view lattice  {:8.2} MB/day → byte budget binds after {view_days:8.0} days\n\
         \x20   scheme 1      {:8.2} MB/day → byte budget binds after {scheme1_days:8.0} days\n",
        view / 1e6,
        scheme1 / 1e6
    );

    // **The finding.** §5.4 argued the fourth state (`"unknown"`: a surviving measurement whose
    // coverage record has expired) would be unreachable in practice, because T-406 lengthened the
    // observation log until it outlived the pyramid's *realised* span. That was measured against
    // scheme 1, whose dense level 0 exhausts 8 GiB in a couple of weeks. A view lattice is three
    // orders of magnitude sparser — a 128 s × 100 kHz cell against a 1 s × 6.25 kHz one — so its
    // byte budget does not bind for **years**, and the 180-day record age binds first at every
    // `(level_f, level_t)`. The fourth state is reachable again, on any installation that runs
    // half a year. T-423 defined it; this is when it fires.
    assert!(
        scheme1_days < record_age_days,
        "scheme 1's byte budget must still bind first: {scheme1_days} days"
    );
    assert!(
        view_days > record_age_days * 4.0,
        "the view lattice's byte budget must not be what binds: {view_days} days"
    );

    // And the corollary for the live edge, which is the cost that is actually paid. The budget is
    // shared, so what the lattice spends on coarse summaries the finest node does not get: under
    // a welded ladder the finest level holds 1/1.5 of the bytes, under the lattice 1/5. **The
    // same byte budget buys the live edge about a third of the history it used to.** That is the
    // price of de-welding, and it is a bounded, stated one rather than a surprise.
    let finest_share_lattice = 1.0 / LATTICE_VS_FINEST;
    let finest_share_ladder = 1.0 / LADDER_VS_FINEST;
    let cut = finest_share_ladder / finest_share_lattice;
    println!(
        "  finest node's share of the budget: ladder {:.0}%, lattice {:.0}% — a {cut:.1}× cut\n",
        finest_share_ladder * 100.0,
        finest_share_lattice * 100.0
    );
    assert!(
        (2.0..5.0).contains(&cut),
        "the live edge's share of a fixed budget should fall by about 3×: {cut}"
    );
}
