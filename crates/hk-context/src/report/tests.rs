//! Report assembly unit tests (T-121) with in-memory providers.

use hk_model::attention::baseline::SiteKey;
use hk_model::attention::occupancy::{
    ChannelKey, OccupancyStat, OccupancySubject, ThresholdSpec, TimingRegime,
};
use hk_model::attention::report::{
    ComparisonStatus, ProvenanceStep, ProvenanceStepKind, ReportEmitter, SurveyReport,
};
use hk_model::{AnomalyId, EmitterId, FreqRange, PowerUnit, TimeRange, Timestamp};
use hk_store::history::{FrontEndState, GainState, ProvenanceStep as TileStep, ProvenanceSummary};
use hk_store::{CellStats, RegionHistory};

use super::*;

fn t(s: f64) -> Timestamp {
    Timestamp::from_unix_nanos((s * 1e9) as i64)
}

const MHZ: f64 = 1e6;

/// Visits: `(freq, start_s, end_s)`; a cell is observed by a visit covering it entirely.
struct FakeCoverage(Option<Vec<(FreqRange, f64, f64)>>);

impl CoverageProvider for FakeCoverage {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn observed(
        &self,
        cells: &[FreqRange],
        _span: TimeRange,
    ) -> Result<Option<Vec<Vec<TimeRange>>>, ReportError> {
        Ok(self.0.as_ref().map(|visits| {
            cells
                .iter()
                .map(|c| {
                    visits
                        .iter()
                        .filter(|(f, _, _)| f.lo_hz <= c.lo_hz && c.hi_hz <= f.hi_hz)
                        .map(|&(_, a, b)| TimeRange::new(t(a), t(b)))
                        .collect()
                })
                .collect()
        }))
    }
}

fn stat(req: &ReportRequest, subject: OccupancySubject, fco: f64) -> OccupancyStat {
    OccupancyStat {
        schema: 1,
        site: req.site,
        subject,
        interval: req.span,
        fco: Some(fco),
        fco_all_visits: Some(fco),
        fco_suspect_upper: None,
        fbo: Some(fco / 2.0),
        sro: None,
        n_revisits: 10,
        n_occupied: (fco * 10.0) as u64,
        n_suspect: 0,
        n_revisits_all: 10,
        observed_s: 10.0,
        revisit_max_s: None,
        revisit_mean_s: None,
        timing: TimingRegime::Unknown,
        threshold: ThresholdSpec::default(),
        threshold_db: -120.0,
        guard_clamped: false,
        rbw_hz: 1000.0,
        obw_hz: None,
        unit: PowerUnit::Dbfs,
        calibration: None,
        confidence: None,
        revisit_biased: false,
        fco_window: None,
        floor_db: None,
        floor_source: None,
        floor_suspect: None,
        level_occupied_p50_db: None,
        level_occupied_p90_db: None,
        level_idle_db: None,
    }
}

/// Channel FCO rises with extent order.
struct FakeOccupancy;

impl OccupancyProvider for FakeOccupancy {
    fn occupancy(
        &self,
        req: &ReportRequest,
        channels: &[FreqRange],
    ) -> Result<OccupancyRows, ReportError> {
        Ok(OccupancyRows {
            bands: vec![stat(req, OccupancySubject::Band { freq: req.region }, 0.3)],
            channels: channels
                .iter()
                .enumerate()
                .map(|(i, &f)| {
                    let key = ChannelKey::snap(1, 1000.0, f).unwrap();
                    (
                        f,
                        stat(
                            req,
                            OccupancySubject::Channel { key },
                            (i + 1) as f64 / (channels.len() + 1) as f64,
                        ),
                    )
                })
                .collect(),
            warnings: vec![],
        })
    }
}

struct FakeInventory(Vec<ReportEmitter>);

impl InventoryProvider for FakeInventory {
    fn emitters(&self, _req: &ReportRequest) -> Result<Vec<ReportEmitter>, ReportError> {
        Ok(self.0.clone())
    }
    fn anomalies(&self, _req: &ReportRequest) -> Result<Vec<AnomalyId>, ReportError> {
        Ok(vec![])
    }
}

struct FakeProvenance(ProvenanceSummary);

impl ProvenanceProvider for FakeProvenance {
    fn steps(
        &self,
        req: &ReportRequest,
    ) -> Result<(Vec<ProvenanceStep>, Vec<String>), ReportError> {
        Ok(steps_from_summary(&self.0, req.span))
    }
}

fn emitter(center_mhz: f64, sightings: u64) -> ReportEmitter {
    ReportEmitter {
        emitter_id: EmitterId::new(),
        freq: FreqRange::centered(center_mhz * MHZ, 25e3),
        first_seen: t(10.0),
        last_seen: t(3000.0),
        sightings,
        lifecycle: "candidate".into(),
        fco: None,
        fco_all_visits: None,
        top_suggestion: Some("land-mobile".into()),
        new_in_span: true,
    }
}

fn request() -> ReportRequest {
    ReportRequest::new(
        FreqRange::new(100.0 * MHZ, 116.0 * MHZ),
        TimeRange::new(t(0.0), t(3600.0)),
        t(3600.0),
    )
}

/// Visits over 100–109 MHz: 1 s every 60 s, except none in [1200, 2400) s; 109–116 MHz never.
fn visits_with_gap() -> Vec<(FreqRange, f64, f64)> {
    (0..60)
        .map(|i| f64::from(i) * 60.0)
        .filter(|&s| !(1200.0..2400.0).contains(&s))
        .map(|s| (FreqRange::new(100.0 * MHZ, 109.0 * MHZ), s, s + 1.0))
        .collect()
}

fn gain_step_summary() -> ProvenanceSummary {
    let g = |lna| GainState {
        lna_db: lna,
        vga_db: 20.0,
        amp_on: false,
    };
    ProvenanceSummary {
        steps: vec![TileStep {
            t: t(1800.0),
            changed: TileStep::GAIN,
            from: FrontEndState {
                gain: Some(g(32.0)),
                ..FrontEndState::default()
            },
            to: FrontEndState {
                gain: Some(g(24.0)),
                ..FrontEndState::default()
            },
        }],
        ..ProvenanceSummary::default()
    }
}

fn build(
    coverage: FakeCoverage,
    emitters: Vec<ReportEmitter>,
) -> Result<SurveyReport, ReportError> {
    let inv = FakeInventory(emitters);
    let prov = FakeProvenance(gain_step_summary());
    assemble(
        &request(),
        &Providers {
            coverage: vec![&coverage],
            occupancy: &FakeOccupancy,
            inventory: &inv,
            baseline: &NoBaselines,
            provenance: &prov,
        },
    )
}

#[test]
fn report_without_coverage_is_refused() {
    let err = build(FakeCoverage(None), vec![emitter(101.0, 5)]).unwrap_err();
    assert!(matches!(err, ReportError::NoCoverage), "{err}");
}

#[test]
fn report_discloses_gap_and_never_calls_it_quiet() {
    let r = build(FakeCoverage(Some(visits_with_gap())), vec![]).unwrap();
    r.validate().unwrap();
    let c = &r.coverage;
    assert!(
        c.observed_fraction > 0.0 && c.observed_fraction < 0.01,
        "{}",
        c.observed_fraction
    );
    assert!(c.statement.contains("not quiet"), "{}", c.statement);
    assert_eq!(c.poi.len(), REPORT_POI_TAUS_S.len());
    assert!(c.poi.iter().all(|p| p.p_poi > 0.0 && p.p_poi < 1.0));
    // The observed cells' gap: the whole 100–109 MHz span from the 1141 s visit's end to 2400 s.
    let g = *c
        .gaps
        .iter()
        .find(|g| (g.freq.lo_hz - 100.0 * MHZ).abs() < 1.0)
        .unwrap();
    assert_eq!(g.time, TimeRange::new(t(1141.0), t(2400.0)), "{:?}", c.gaps);
    assert!((g.freq.lo_hz - 100.0 * MHZ).abs() < 1.0 && (g.freq.hi_hz - 109.0 * MHZ).abs() < 1.0);
    // 109–116 MHz: never observed, and its whole span is the longest gap.
    assert_eq!(c.never_observed.len(), 1);
    assert!((c.never_observed[0].lo_hz - 109.0 * MHZ).abs() < 1.0);
    assert!(c.gaps[0].freq.lo_hz >= 109.0 * MHZ - 1.0 && c.gaps[0].time == r.span);
    let json = serde_json::to_string(&r).unwrap();
    assert_eq!(serde_json::from_str::<SurveyReport>(&json).unwrap(), r);
}

#[test]
fn report_lists_provenance_gain_step() {
    let r = build(FakeCoverage(Some(visits_with_gap())), vec![]).unwrap();
    assert_eq!(r.provenance_steps.len(), 1, "{:?}", r.provenance_steps);
    let s = &r.provenance_steps[0];
    assert_eq!((s.kind, s.t), (ProvenanceStepKind::Gain, t(1800.0)));
    assert!(s.detail.contains("lna 32→24 dB"), "{}", s.detail);
    // Outside the span: not listed.
    let (steps, _) = steps_from_summary(&gain_step_summary(), TimeRange::new(t(0.0), t(60.0)));
    assert!(steps.is_empty());
}

/// T-332: a bias-tee switch is listed beside gain, calibration, spur mask and antenna port, so the
/// explain-the-device-first path has something to name. The **control** is a summary whose
/// bias-tee state never changes: no such step, so the step cannot pass by being emitted always.
#[test]
fn report_lists_bias_tee_step_only_when_the_tee_actually_moved() {
    use hk_model::BiasTee;

    let summary = |from: BiasTee, to: BiasTee| ProvenanceSummary {
        steps: vec![TileStep {
            t: t(1800.0),
            changed: TileStep::BIAS_TEE,
            from: FrontEndState {
                bias_tee: from,
                ..FrontEndState::default()
            },
            to: FrontEndState {
                bias_tee: to,
                ..FrontEndState::default()
            },
        }],
        ..ProvenanceSummary::default()
    };
    let span = TimeRange::new(t(0.0), t(3600.0));

    let (steps, _) = steps_from_summary(&summary(BiasTee::Off, BiasTee::On), span);
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert_eq!(
        (steps[0].kind, steps[0].t),
        (ProvenanceStepKind::BiasTee, t(1800.0))
    );
    assert_eq!(steps[0].detail, "bias tee off→on");
    // The wire form is the kebab-case name the API documents, beside `antenna-port`.
    let json = serde_json::to_value(&steps[0]).unwrap();
    assert_eq!(json["kind"], "bias-tee");

    // Unknown is a state of its own, so learning it is still a step (what it *means* is decided in
    // the alarm path: a change of knowledge never explains a level change away).
    let (steps, _) = steps_from_summary(&summary(BiasTee::Unknown, BiasTee::On), span);
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert_eq!(steps[0].detail, "bias tee unknown→on");

    // Control: no change, no step — not for a run that held the tee, and not for a pre-T-332 tile
    // whose step record has no bias-tee byte to have changed.
    for held in [BiasTee::Off, BiasTee::On, BiasTee::Unknown] {
        let (steps, _) = steps_from_summary(&summary(held, held), span);
        assert!(steps.is_empty(), "{held:?}: {steps:?}");
    }
    let (steps, _) = steps_from_summary(&ProvenanceSummary::default(), span);
    assert!(steps.is_empty(), "{steps:?}");
}

#[test]
fn report_marks_baseline_unavailable() {
    let r = build(
        FakeCoverage(Some(visits_with_gap())),
        vec![emitter(101.0, 3)],
    )
    .unwrap();
    assert_eq!(r.change_vs_baseline.status, ComparisonStatus::Unavailable);
    assert!(r.change_vs_baseline.changes.is_empty() && r.change_vs_baseline.baseline.is_none());
    assert!(
        r.warnings.iter().any(|w| w.contains("unavailable")),
        "{:?}",
        r.warnings
    );
}

#[test]
fn report_top_emitters_carry_channel_fco_and_caps() {
    let mut emitters: Vec<ReportEmitter> = (0..30)
        .map(|i| emitter(100.1 + 0.3 * f64::from(i), 100 - i as u64))
        .collect();
    // Two overlapping extents merge into one channel.
    emitters.push(emitter(100.105, 1));
    let r = build(FakeCoverage(Some(visits_with_gap())), emitters).unwrap();
    assert_eq!(r.top_emitters.len(), DEFAULT_MAX_EMITTERS);
    assert_eq!(r.top_emitters[0].sightings, 100);
    assert!(r.top_emitters.iter().all(|e| e.fco.is_some()));
    assert_eq!(r.occupancy.channels.len(), 30);
    assert!(!r.occupancy.truncated);
    let fcos: Vec<f64> = r
        .occupancy
        .channels
        .iter()
        .map(|s| s.fco.unwrap())
        .collect();
    assert!(
        fcos.windows(2).all(|w| w[0] >= w[1]),
        "FCO descending: {fcos:?}"
    );
    assert_eq!(r.occupancy.bands.len(), 1);
    assert_eq!(r.site, SiteKey::Unassigned);
}

fn grid() -> RegionHistory {
    let observed = CellStats {
        max_db: -100.0,
        mean_db: -110.0,
        p_low_db: -120.0,
        p_high_db: -105.0,
        occupancy: 0.5,
        occupancy_max: 0.5,
        coverage: 0.01,
        floor_db: -121.0,
        frames: 4,
        level: 0,
    };
    let (nt, nf) = (12, 4);
    RegionHistory {
        scheme: 1,
        level: 0,
        unit: PowerUnit::Dbfs,
        f_cell_hz: 4.0 * MHZ,
        f_first_cell: 25,
        nf,
        t_cell_ns: 300_000_000_000,
        t_first_cell: 0,
        nt,
        percentiles: (10.0, 90.0),
        cells: (0..nt * nf)
            .map(|i| {
                if (4..8).contains(&(i / nf)) {
                    CellStats::NONE
                } else {
                    observed
                }
            })
            .collect(),
        provenance: gain_step_summary(),
        tiles_read: 1,
        filter: None,
    }
}

#[test]
fn report_from_history_tiles_and_exports() {
    let tiles = HistoryTiles::from_grid(grid(), 4.0 * MHZ, 6.0);
    let inv = FakeInventory(vec![emitter(101.0, 4)]);
    let r = assemble(
        &request(),
        &Providers {
            coverage: vec![&FakeCoverage(None), &tiles],
            occupancy: &tiles,
            inventory: &inv,
            baseline: &NoBaselines,
            provenance: &tiles,
        },
    )
    .unwrap();
    // Rows 4–7 (1200–2400 s) are unobserved: a full-width gap, disclosed, never quiet.
    assert!(
        (r.coverage.observed_fraction - 8.0 * 3.0 / 3600.0).abs() < 1e-9,
        "{}",
        r.coverage.observed_fraction
    );
    // A tile row counts as observed from its start for coverage × row (row 3: 900 s + 3 s).
    let g = r.coverage.gaps[0];
    assert!(
        (g.time.start.as_unix_nanos() - t(903.0).as_unix_nanos()).abs() < 1_000_000,
        "{g:?}"
    );
    assert_eq!(g.time.end, t(2400.0));
    assert!(
        (g.freq.width_hz() - 16.0 * MHZ).abs() < 1.0,
        "full width: {g:?}"
    );
    assert!(r.warnings.iter().any(|w| w.contains("history tiles")));
    assert_eq!(r.provenance_steps[0].kind, ProvenanceStepKind::Gain);
    let band = &r.occupancy.bands[0];
    // Tile rows are all-visits: 8 observed rows, all occupied, and no unbiased figure.
    assert_eq!(
        (
            band.n_revisits_all,
            band.n_revisits,
            band.fco,
            band.fco_all_visits
        ),
        (8, 0, None, Some(1.0))
    );
    assert_eq!(r.occupancy.channels.len(), 1);

    let csv = report_csv(&r, 4.0 * MHZ);
    assert!(
        csv.lines()
            .any(|l| l.starts_with("# coverage:") && l.contains("not quiet")),
        "{csv}"
    );
    assert!(csv.lines().any(|l| l.starts_with("band,")));
    assert!(csv.lines().any(|l| l.starts_with("channel,")));
    assert!(
        csv.lines()
            .any(|l| l.starts_with("gap,100000000,116000000,903.000,2400.000")),
        "{csv}"
    );
    let png = report_png(tiles.grid());
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(png.len() > 60);
}

/// T-133: a source/site-filtered grid says what it filtered and excluded; an unfiltered grid for a
/// site report says its rows include every site.
#[test]
fn report_discloses_history_source_site_filter() {
    use hk_store::history::{FilterSummary, OriginField, OriginFilter};
    let inv = FakeInventory(vec![emitter(101.0, 4)]);
    let site = SiteKey::Site(hk_model::ids::SiteId::new());
    let mut req = request();
    req.site = site;
    let build = |grid: RegionHistory| {
        let tiles = HistoryTiles::from_grid(grid, 4.0 * MHZ, 6.0);
        assemble(
            &req,
            &Providers {
                coverage: vec![&tiles],
                occupancy: &tiles,
                inventory: &inv,
                baseline: &NoBaselines,
                provenance: &tiles,
            },
        )
        .unwrap()
    };
    let r = build(grid());
    assert!(
        r.warnings
            .iter()
            .any(|w| w.contains("include every source and site")),
        "{:?}",
        r.warnings
    );
    let mut g = grid();
    g.filter = Some(FilterSummary {
        filter: OriginFilter {
            source: OriginField::Any,
            site: OriginField::Is(site),
        },
        tiles_matched: 3,
        tiles_mixed: 1,
        tiles_other: 0,
        cells_excluded: 16,
        cells_from_children: 4,
    });
    let r = build(g);
    let w = r
        .warnings
        .iter()
        .find(|w| w.contains("history tiles filtered"))
        .unwrap_or_else(|| panic!("{:?}", r.warnings));
    let SiteKey::Site(id) = site else {
        unreachable!()
    };
    assert!(w.contains(&format!("source any and site {id}")), "{w}");
    assert!(
        w.contains("16 observed cells") && w.contains("not quiet"),
        "{w}"
    );
    assert!(
        !r.warnings
            .iter()
            .any(|w| w.contains("include every source"))
    );
}

/// ADR-0012 §2.5: the history-tile stand-in reports its ratio as `fco_all_visits` only, never as
/// unbiased `fco`, in the occupancy rows, the top emitters and the CSV.
#[test]
fn report_tile_stand_in_never_carries_unbiased_fco() {
    let tiles = HistoryTiles::from_grid(grid(), 4.0 * MHZ, 6.0);
    let inv = FakeInventory(vec![emitter(101.0, 4), emitter(109.0, 2)]);
    let r = assemble(
        &request(),
        &Providers {
            coverage: vec![&tiles],
            occupancy: &tiles,
            inventory: &inv,
            baseline: &NoBaselines,
            provenance: &tiles,
        },
    )
    .unwrap();
    let rows: Vec<&OccupancyStat> = r
        .occupancy
        .bands
        .iter()
        .chain(&r.occupancy.channels)
        .collect();
    assert_eq!(rows.len(), 3);
    for s in rows {
        assert_eq!(s.fco, None, "{s:?}");
        assert!(s.fco_all_visits.is_some() && s.revisit_biased, "{s:?}");
        assert!(
            matches!(
                s.threshold.method,
                hk_model::attention::occupancy::ThresholdMethod::HistoryTile { margin_db } if margin_db == 6.0
            ),
            "{:?}",
            s.threshold
        );
    }
    assert!(!r.top_emitters.is_empty());
    for e in &r.top_emitters {
        assert_eq!(e.fco, None, "{e:?}");
        assert!(e.fco_all_visits.is_some(), "{e:?}");
    }
    assert!(r.warnings.iter().any(|w| w.contains("no unbiased fco")));
    let csv = report_csv(&r, 4.0 * MHZ);
    let header = csv.lines().find(|l| l.starts_with("row,")).unwrap();
    assert!(header.contains(",fco,fco_all_visits,"), "{header}");
    let band = csv.lines().find(|l| l.starts_with("band,")).unwrap();
    let cols: Vec<&str> = band.split(',').collect();
    assert_eq!(cols.len(), header.split(',').count(), "{band}");
    assert_eq!(
        (cols[5], cols[6], cols[12]),
        ("", "1.000000", "true"),
        "{band}"
    );
}

/// A coverage source that starts mid-span says the time before it is shown as unobserved.
#[test]
fn report_partial_log_warns_time_before_is_unobserved() {
    let late: Vec<_> = visits_with_gap()
        .into_iter()
        .filter(|&(_, s, _)| s >= 2400.0)
        .collect();
    let r = build(FakeCoverage(Some(late)), vec![emitter(101.0, 4)]).unwrap();
    let w = r
        .warnings
        .iter()
        .find(|w| w.starts_with("coverage and POI from fake"))
        .unwrap();
    assert!(
        w.contains("before 2400.000 s") && w.contains("shown as unobserved, not quiet"),
        "{w}"
    );
    let full = build(FakeCoverage(Some(visits_with_gap())), vec![]).unwrap();
    assert!(
        full.warnings
            .iter()
            .any(|w| w == "coverage and POI from fake")
    );
}

fn hours(h: i64) -> TimeRange {
    // On a whole hour (so on every finer level's cell edge).
    let t0 = 1_699_999_200_000_000_000i64;
    TimeRange::new(
        Timestamp::from_unix_nanos(t0),
        Timestamp::from_unix_nanos(t0 + h * 3_600_000_000_000),
    )
}

/// A normal 48 h × 20 MHz report on the default history ladder is a bounded grid: level 2
/// (15 min × 25 kHz), 192 × 800 = 153 600 cells (≤ 200 000), read in one lock chunk.
#[test]
fn report_grid_48h_20mhz_is_bounded() {
    let geom = hk_store::PyramidConfig::default().geometry().unwrap();
    let (region, span) = (FreqRange::new(90.0 * MHZ, 110.0 * MHZ), hours(48));
    let (level, chunks) = report_chunks(&geom, region, span).unwrap();
    let (nt, nf) = report_grid_dims(&geom, level, region, span);
    assert_eq!((level, nt, nf), (2, 192.0, 800.0));
    assert!(nt * nf <= 200_000.0 && nt * nf <= REPORT_MAX_CELLS as f64);
    assert_eq!(chunks.len(), 1);
}

#[test]
fn report_oversized_box_is_invalid() {
    let geom = hk_store::PyramidConfig::default().geometry().unwrap();
    for (region, span) in [
        (FreqRange::new(1.0, 1e12), hours(48)),
        (FreqRange::new(1.0 * MHZ, 6000.0 * MHZ), hours(24 * 3650)),
        (FreqRange::new(2.0, 1.0), hours(1)),
    ] {
        assert!(
            matches!(
                report_chunks(&geom, region, span),
                Err(ReportError::Invalid(_))
            ),
            "{region:?} {span:?}"
        );
    }
}

/// Lock chunks tile the span without gaps or overlap, on whole tiles of the chosen level.
#[test]
fn report_chunks_tile_the_span() {
    let geom = hk_store::PyramidConfig::default().geometry().unwrap();
    let base = hours(240);
    let span = TimeRange::new(base.start.saturating_add_nanos(123_456_789), base.end);
    let (level, chunks) =
        report_chunks(&geom, FreqRange::new(100.0 * MHZ, 100.1 * MHZ), span).unwrap();
    let g = geom.levels[level];
    assert!(chunks.len() >= 3, "{}", chunks.len());
    assert_eq!(chunks[0].start, span.start);
    assert_eq!(chunks.last().unwrap().end, span.end);
    for w in chunks.windows(2) {
        assert_eq!(w[0].end, w[1].start);
        assert_eq!(w[0].end.as_unix_nanos() % g.t_block_ns(), 0);
    }
    assert!(
        chunks.iter().all(|c| c.duration_ns()
            <= (REPORT_LOCK_CHUNK_ROWS.div_ceil(g.nt) * g.nt) as i64 * g.t_cell_ns)
    );
}
