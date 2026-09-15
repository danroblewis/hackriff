//! Survey report wiring (T-121, ADR-0012 §6): the API-facing report provider over the stores the
//! running pipeline owns.
//!
//! [`ReportService`] reads the spectrum history (the history store, else the floor product's
//! uncalibrated pyramid, as `/api/history` does), the observation log (T-115) and the inventory
//! database, and assembles `report(region, span)` through `hk_context::report`. Coverage comes
//! from the observation log when it holds visits for the box, else from history tiles.
//!
//! **Occupancy and baselines (T-128).** With the run's occupancy engine attached
//! ([`ReportService::with_attention`]), occupancy comes from T-118's stored 15-min series
//! ([`SeriesOccupancy`]): rows overlapping the span (whole rows), of the report's site when it is
//! a site, rolled up per subject by summing counts and weighting each FCO by its own visit counts
//! (§2.8), so channel rows carry the true `fco` (activity-independent, suspect excluded) where the
//! engine had such visits. Without series rows for the box the history-tile stand-in answers
//! (`fco_all_visits` only) with a warning. With the attention service attached the change vs
//! baseline is T-119's comparison ([`AttentionService::compare_report`]); otherwise it stays
//! `unavailable`.
//!
//! **Ingest lock.** The history writer only `try_lock`s its store and drops frames while the lock
//! is held, so the report grid is bounded ([`report_chunks`]: ≤ `REPORT_MAX_CELLS`, else 400) and
//! read in chunks of ≤ `REPORT_LOCK_CHUNK_ROWS` rows, each under its own short lock scope. One grid
//! serves JSON, CSV and PNG; assembly runs with the history lock released.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

pub use hk_context::report::ReportError;
use hk_context::report::{
    self, BaselineProvider, CoverageProvider, HistoryTiles, NoBaselines, ObservationCoverage,
    OccupancyProvider, OccupancyRows, Providers, RepoInventory, ReportRequest, report_chunks,
    report_csv, report_png,
};
use hk_model::attention::occupancy::{
    ConfidenceLevel, OccupancyStat, OccupancySubject, fraction_interval,
};
use hk_model::attention::report::{BaselineComparison, ReportOccupancy};
use hk_store::occupancy::{OccupancyQuery, SeriesInterval};

use crate::attention::{AttentionService, represented_s};
use crate::occupancy::OccupancyService;

/// Most series rows one report reads.
pub const REPORT_MAX_SERIES_ROWS: usize = 200_000;
use hk_model::attention::baseline::SiteKey;
use hk_model::attention::report::{ExportFormat, SurveyReport};
use hk_model::{FreqRange, Repository, TimeRange, Timestamp};
use hk_store::observation::ObservationStore;
use hk_store::{FloorProduct, Pyramid};

/// Builds survey reports from the run's stores. Cheap to clone.
#[derive(Clone)]
pub struct ReportService {
    history: Option<Arc<Mutex<Pyramid>>>,
    floor: Option<Arc<Mutex<FloorProduct>>>,
    observations: Option<ObservationStore>,
    inventory: Arc<Mutex<Repository>>,
    occupancy: Option<Arc<OccupancyService>>,
    attention: Option<Arc<AttentionService>>,
}

impl ReportService {
    /// A service over a history store (or, when `None`, the floor product's uncalibrated
    /// pyramid), the observation log when there is one, and the inventory database.
    pub fn new(
        history: Option<Arc<Mutex<Pyramid>>>,
        floor: Option<Arc<Mutex<FloorProduct>>>,
        observations: Option<ObservationStore>,
        inventory: Arc<Mutex<Repository>>,
    ) -> Self {
        Self {
            history,
            floor,
            observations,
            inventory,
            occupancy: None,
            attention: None,
        }
    }

    /// T-128: the run's occupancy engine (series rows, true `fco`) and attention service
    /// (baseline comparison) as the report's occupancy and baseline providers.
    pub fn with_attention(
        mut self,
        occupancy: Option<Arc<OccupancyService>>,
        attention: Option<Arc<AttentionService>>,
    ) -> Self {
        self.occupancy = occupancy;
        self.attention = attention;
        self
    }

    /// Runs `f` under one short history-lock scope.
    fn with_pyramid<T>(
        &self,
        f: impl FnOnce(&Pyramid) -> Result<T, ReportError>,
    ) -> Result<T, ReportError> {
        let poisoned = || ReportError::Provider("history store poisoned".into());
        if let Some(p) = &self.history {
            return f(&*p.lock().map_err(|_| poisoned())?);
        }
        if let Some(fl) = &self.floor {
            return f(fl.lock().map_err(|_| poisoned())?.uncalibrated_pyramid());
        }
        // No history: nothing can disclose coverage, so no report (§6.2).
        Err(ReportError::NoCoverage)
    }

    /// `report(region, span)` for `site`.
    pub fn report(
        &self,
        region: FreqRange,
        span: TimeRange,
        site: SiteKey,
    ) -> Result<SurveyReport, ReportError> {
        self.build(region, span, site).map(|(r, _)| r)
    }

    /// The report's export: `(content type, body)`.
    pub fn export(
        &self,
        region: FreqRange,
        span: TimeRange,
        site: SiteKey,
        format: ExportFormat,
    ) -> Result<(&'static str, Vec<u8>), ReportError> {
        let (r, tiles) = self.build(region, span, site)?;
        Ok(match format {
            ExportFormat::Json => (
                "application/json",
                serde_json::to_vec(&r).map_err(|e| ReportError::Provider(e.to_string()))?,
            ),
            ExportFormat::Csv => (
                "text/csv; charset=utf-8",
                report_csv(&r, tiles.level0_f_cell_hz()).into_bytes(),
            ),
            ExportFormat::Png => ("image/png", report_png(tiles.grid())),
        })
    }

    /// The bounded report grid, read chunk by chunk with the history lock released in between
    /// (`between` runs at each release; tests use it to prove ingest can take the lock), and the
    /// stream time history has reached (ADR-0012 §0: the sample clock, never the wall clock).
    fn query_tiles(
        &self,
        region: FreqRange,
        span: TimeRange,
        between: &mut dyn FnMut(),
    ) -> Result<(HistoryTiles, Timestamp), ReportError> {
        let (geom, margin_db) = self.with_pyramid(|p| {
            Ok((
                p.geometry().clone(),
                f64::from(p.config().occupancy_margin_db),
            ))
        })?;
        let (level, chunks) = report_chunks(&geom, region, span)?;
        let mut parts = Vec::with_capacity(chunks.len());
        let mut generated_at = span.end;
        for (i, chunk) in chunks.into_iter().enumerate() {
            if i > 0 {
                between();
            }
            let (grid, latest) = self.with_pyramid(|p| {
                Ok((
                    HistoryTiles::query_level(p, region, chunk, level)?,
                    p.latest_frame_end(),
                ))
            })?;
            generated_at = latest.unwrap_or(span.end);
            parts.push(grid);
        }
        let tiles = HistoryTiles::from_parts(parts, geom.levels[0].f_cell_hz, margin_db)?;
        Ok((tiles, generated_at))
    }

    fn build(
        &self,
        region: FreqRange,
        span: TimeRange,
        site: SiteKey,
    ) -> Result<(SurveyReport, HistoryTiles), ReportError> {
        // `report_chunks` runs the one region/span check before any store is read.
        let (tiles, generated_at) = self.query_tiles(region, span, &mut || {})?;
        let repo = self
            .inventory
            .lock()
            .map_err(|_| ReportError::Provider("inventory database poisoned".into()))?;
        let inventory = RepoInventory(&repo);
        let log = self.observations.as_ref().map(ObservationCoverage);
        let mut coverage: Vec<&dyn CoverageProvider> = Vec::with_capacity(2);
        if let Some(log) = &log {
            coverage.push(log);
        }
        coverage.push(&tiles);
        let mut req = ReportRequest::new(region, span, generated_at);
        req.site = site;
        // T-128: T-118's series and T-119's comparison when the run attached them.
        let series = self.occupancy.as_deref().map(|svc| SeriesOccupancy {
            svc,
            fallback: &tiles,
            channel_rows: Mutex::new(None),
        });
        let occupancy: &dyn OccupancyProvider = match &series {
            Some(s) => s,
            None => &tiles,
        };
        let baselines = self
            .attention
            .as_deref()
            .map(|attention| AttentionBaselines {
                attention,
                series: series.as_ref(),
            });
        let baseline: &dyn BaselineProvider = match &baselines {
            Some(b) => b,
            None => &NoBaselines,
        };
        let r = report::assemble(
            &req,
            &Providers {
                coverage,
                occupancy,
                inventory: &inventory,
                baseline,
                provenance: &tiles,
            },
        )?;
        drop(repo);
        Ok((r, tiles))
    }
}

/// T-128: the occupancy engine's stored 15-min series as the report's occupancy.
pub struct SeriesOccupancy<'a> {
    /// The run's occupancy engine.
    pub svc: &'a OccupancyService,
    /// Answers when the series holds nothing for the box.
    pub fallback: &'a dyn OccupancyProvider,
    /// The per-interval channel rows the last `occupancy` call rolled up (`None` when it fell
    /// back), for the per-interval baseline comparison.
    pub channel_rows: Mutex<Option<Vec<OccupancyStat>>>,
}

/// Grouping key of a series row: channel cells, or a band's extent in Hz.
fn subject_group(s: &OccupancySubject) -> (u8, i64, i64) {
    match s {
        OccupancySubject::Channel { key } => (0, key.lo_cell, key.hi_cell),
        OccupancySubject::Band { freq } => {
            (1, freq.lo_hz.round() as i64, freq.hi_hz.round() as i64)
        }
    }
}

fn weighted(
    rows: &[&OccupancyStat],
    v: impl Fn(&OccupancyStat) -> Option<f64>,
    w: impl Fn(&OccupancyStat) -> f64,
) -> Option<f64> {
    let (mut sw, mut sv) = (0.0, 0.0);
    for r in rows {
        if let Some(x) = v(r).filter(|x| x.is_finite()) {
            let k = w(r);
            sw += k;
            sv += k * x;
        }
    }
    (sw > 0.0).then(|| sv / sw)
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    v.retain(|x| x.is_finite());
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    Some(v[v.len() / 2])
}

/// One subject's 15-min rows rolled up over `span` (§2.8: counts summed; the confidence interval
/// recomputed from the summed `n_eff`). `fco` and `fco_suspect_upper` are weighted by the time
/// each row represents × its usable share (§2.5, as the baseline fold weighs a row), never by visit
/// count, so intervals where the bandit thinned the sweep visits are not underweighted. `fbo`/`sro`
/// weigh by observed time, `fco_all_visits` (information only) by all visits. `fco_window` is
/// dropped; levels and floors are the rows' medians.
pub fn roll_up(rows: &[&OccupancyStat], span: TimeRange) -> Option<OccupancyStat> {
    let first = rows.first()?;
    let mut out = (*first).clone();
    let sum = |f: fn(&OccupancyStat) -> u64| rows.iter().map(|r| f(r)).sum::<u64>();
    out.interval = span;
    out.n_revisits = sum(|r| r.n_revisits);
    out.n_occupied = sum(|r| r.n_occupied);
    out.n_suspect = sum(|r| r.n_suspect);
    out.n_revisits_all = sum(|r| r.n_revisits_all);
    out.observed_s = rows.iter().map(|r| r.observed_s).sum();
    let usable = |r: &OccupancyStat| r.n_revisits.saturating_sub(r.n_suspect) as f64;
    let time_usable = |r: &OccupancyStat| {
        if r.n_revisits == 0 {
            return 0.0;
        }
        represented_s(r) * usable(r) / r.n_revisits as f64
    };
    out.fco = weighted(rows, |r| r.fco, time_usable);
    out.fco_all_visits = weighted(rows, |r| r.fco_all_visits, |r| r.n_revisits_all as f64);
    out.fco_suspect_upper = weighted(rows, |r| r.fco_suspect_upper, represented_s);
    // §2.8: the interval from the summed n_eff of the rows `fco` was computed from.
    out.confidence = out.fco.and_then(|p| {
        let with_fco = || rows.iter().filter(|r| r.fco.is_some());
        let n_eff: f64 = with_fco()
            .map(|r| r.confidence.map_or(usable(r), |c| c.n_eff))
            .sum();
        let level = with_fco()
            .find_map(|r| r.confidence.map(|c| c.level))
            .unwrap_or(ConfidenceLevel::P95);
        let assumed = with_fco().any(|r| r.confidence.is_none_or(|c| c.independence_assumed));
        fraction_interval(p, n_eff, level, assumed)
    });
    out.fbo = weighted(rows, |r| r.fbo, |r| r.observed_s.max(1e-9));
    out.sro = weighted(rows, |r| r.sro, |r| r.observed_s.max(1e-9));
    out.revisit_max_s = rows.iter().filter_map(|r| r.revisit_max_s).reduce(f64::max);
    out.revisit_mean_s = weighted(rows, |r| r.revisit_mean_s, |r| r.n_revisits_all as f64);
    out.threshold_db =
        median(rows.iter().map(|r| r.threshold_db).collect()).unwrap_or(first.threshold_db);
    out.guard_clamped = rows.iter().any(|r| r.guard_clamped);
    out.revisit_biased = rows.iter().any(|r| r.revisit_biased);
    out.fco_window = None;
    out.floor_db = median(rows.iter().filter_map(|r| r.floor_db).collect());
    out.floor_suspect = rows
        .iter()
        .filter_map(|r| r.floor_suspect)
        .reduce(|a, b| a || b);
    out.level_occupied_p50_db = median(
        rows.iter()
            .filter_map(|r| r.level_occupied_p50_db)
            .collect(),
    );
    out.level_occupied_p90_db = median(
        rows.iter()
            .filter_map(|r| r.level_occupied_p90_db)
            .collect(),
    );
    out.level_idle_db = median(rows.iter().filter_map(|r| r.level_idle_db).collect());
    out.validate().ok()?;
    Some(out)
}

impl OccupancyProvider for SeriesOccupancy<'_> {
    fn occupancy(
        &self,
        req: &ReportRequest,
        channels: &[FreqRange],
    ) -> Result<OccupancyRows, ReportError> {
        let (_, f_cell) = self.svc.plan_info();
        let q = OccupancyQuery {
            freq: req.region,
            span: req.span,
            interval: SeriesInterval::Min15,
            subject: None,
            f_cell_hz: f_cell,
            limit: REPORT_MAX_SERIES_ROWS,
        };
        let stored = self.svc.query(&q).unwrap_or_default();
        let by_site = matches!(req.site, SiteKey::Site(_));
        let mut groups: BTreeMap<(u8, i64, i64), Vec<&OccupancyStat>> = BTreeMap::new();
        for r in &stored.rows {
            let overlaps = r.interval.start < req.span.end && r.interval.end > req.span.start;
            if overlaps && (!by_site || r.site == req.site) {
                groups.entry(subject_group(&r.subject)).or_default().push(r);
            }
        }
        if groups.is_empty() {
            let mut rows = self.fallback.occupancy(req, channels)?;
            rows.warnings.push(
                "the occupancy engine holds no series rows for this box yet: history-tile                  stand-in"
                    .into(),
            );
            return Ok(rows);
        }
        let span = TimeRange::new(
            groups
                .values()
                .flatten()
                .map(|r| r.interval.start)
                .min()
                .unwrap_or(req.span.start),
            groups
                .values()
                .flatten()
                .map(|r| r.interval.end)
                .max()
                .unwrap_or(req.span.end),
        );
        let mut out = OccupancyRows::default();
        let n_rows: usize = groups.values().map(Vec::len).sum();
        let in_region = |r: &OccupancyStat| match r.subject {
            OccupancySubject::Channel { key } => {
                let (lo, hi) = (key.lo_cell as f64 * f_cell, key.hi_cell as f64 * f_cell);
                hi > req.region.lo_hz && lo < req.region.hi_hz
            }
            OccupancySubject::Band { .. } => false,
        };
        *self
            .channel_rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(
            groups
                .values()
                .flatten()
                .filter(|r| in_region(r))
                .map(|r| (*r).clone())
                .collect(),
        );
        for rows in groups.values() {
            let Some(stat) = roll_up(rows, span) else {
                continue;
            };
            match stat.subject {
                OccupancySubject::Band { .. } => out.bands.push(stat),
                OccupancySubject::Channel { key } => {
                    let f =
                        FreqRange::new(key.lo_cell as f64 * f_cell, key.hi_cell as f64 * f_cell);
                    if f.hi_hz > req.region.lo_hz && f.lo_hz < req.region.hi_hz {
                        out.channels.push((f, stat));
                    }
                }
            }
        }
        out.warnings.push(format!(
            "occupancy from the occupancy engine's 15-min series: {n_rows} rows over {} subjects              rolled up over {:.2} h (whole rows overlapping the span); fco counts              activity-independent visits only, suspect excluded{}{}",
            groups.len(),
            span.duration_ns() as f64 / 3.6e12,
            if stored.truncated { "; series truncated" } else { "" },
            if by_site { "" } else { "; rows of every site" },
        ));
        Ok(out)
    }
}

/// T-128: T-119's baselines as the report's change vs baseline, compared per 15-min series row
/// (the rows [`SeriesOccupancy`] rolled up) rather than per span rollup.
pub struct AttentionBaselines<'a> {
    /// The run's attention service.
    pub attention: &'a AttentionService,
    /// The series provider whose per-interval rows are compared, when attached.
    pub series: Option<&'a SeriesOccupancy<'a>>,
}

impl BaselineProvider for AttentionBaselines<'_> {
    fn compare(
        &self,
        req: &ReportRequest,
        occupancy: &ReportOccupancy,
    ) -> Result<BaselineComparison, ReportError> {
        let rows = self.series.and_then(|s| {
            s.channel_rows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
        });
        // Without series rows the channels are span rollups, which `compare_report` skips.
        let rows = rows.as_deref().unwrap_or(&occupancy.channels);
        Ok(self.attention.compare_report(req.site, rows))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use hk_context::report::REPORT_MAX_CELLS;
    use hk_store::PyramidConfig;

    use super::*;

    const HOUR_NS: i64 = 3_600_000_000_000;

    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn service(name: &str) -> (Dir, Arc<Mutex<Pyramid>>, ReportService) {
        let dir =
            Dir(std::env::temp_dir()
                .join(format!("hk-pipeline-report-{name}-{}", std::process::id())));
        let history = Arc::new(Mutex::new(
            Pyramid::open(&dir.0, PyramidConfig::default()).unwrap(),
        ));
        let repo = Arc::new(Mutex::new(Repository::open_in_memory().unwrap()));
        let svc = ReportService::new(Some(history.clone()), None, None, repo);
        (dir, history, svc)
    }

    fn span_h(h: i64) -> TimeRange {
        // On a whole hour, so 240 h is exactly 960 level-2 rows.
        let t0 = 1_699_999_200_000_000_000;
        TimeRange::new(
            Timestamp::from_unix_nanos(t0),
            Timestamp::from_unix_nanos(t0 + h * HOUR_NS),
        )
    }

    #[test]
    fn report_oversized_box_is_invalid_not_built() {
        let (_d, _h, svc) = service("oversized");
        for (region, span) in [
            // 1 Hz..1 THz over 48 h: millions of cells even at the top level.
            (FreqRange::new(1.0, 1e12), span_h(48)),
            // 6 GHz over ten years.
            (FreqRange::new(1e6, 6e9), span_h(24 * 3650)),
        ] {
            let e = svc.report(region, span, SiteKey::Unassigned).unwrap_err();
            assert!(matches!(e, ReportError::Invalid(_)), "{e:?}");
            let e = svc
                .export(region, span, SiteKey::Unassigned, ExportFormat::Png)
                .unwrap_err();
            assert!(matches!(e, ReportError::Invalid(_)), "{e:?}");
        }
    }

    /// T-128 item 5 (review): series rows roll up by summing counts and weighting each FCO by the
    /// time it represents (§2.5), not by its visit count; the interval is recomputed from the
    /// summed n_eff (§2.8); a row that validates yields a validating span row.
    #[test]
    fn report_series_roll_up_weights_fco_by_time() {
        use hk_model::attention::occupancy::ChannelKey;
        let key = ChannelKey {
            scheme: 1,
            lo_cell: 69_000,
            hi_cell: 69_004,
        };
        let subject = OccupancySubject::Channel { key };
        let mut a = crate::attention::tests::series_row(0, SiteKey::Unassigned, subject, 0.25);
        let b = crate::attention::tests::series_row(1, SiteKey::Unassigned, subject, 0.75);
        // Both rows represent the whole 15 min; `a` was swept three times as often (25 s apart)
        // while the bandit took most of `b`'s schedule (75 s apart).
        a.n_revisits = 36;
        a.n_revisits_all = 36;
        a.n_occupied = 9;
        a.revisit_mean_s = Some(25.0);
        let span = TimeRange::new(a.interval.start, b.interval.end);
        let r = roll_up(&[&a, &b], span).expect("valid");
        assert_eq!((r.n_revisits, r.n_occupied, r.n_revisits_all), (48, 18, 48));
        let count_weighted = (0.25 * 36.0 + 0.75 * 12.0) / 48.0;
        let ci = r.confidence.expect("recomputed from summed n_eff");
        println!(
            "T-128 roll-up: time-weighted fco {:?} (count-weighted would be {count_weighted}); \
             CI [{:.3}, {:.3}] n_eff {}",
            r.fco, ci.lo, ci.hi, ci.n_eff
        );
        // (0.25·900 + 0.75·900) / 1800
        assert!((r.fco.unwrap() - 0.5).abs() < 1e-12, "{:?}", r.fco);
        assert_eq!(r.interval, span);
        assert_eq!(ci.n_eff, 48.0);
        assert!(ci.lo < 0.5 && ci.hi > 0.5 && ci.hi - ci.lo < 0.3);
        r.validate().unwrap();
    }

    /// A report build releases the history lock between bounded chunks, so ingest (which only
    /// `try_lock`s) is never locked out for the whole build.
    #[test]
    fn report_releases_history_lock_between_chunks() {
        let (_d, history, svc) = service("lock");
        // 10 days × 100 kHz: level 2 (15 min rows), 960 rows → several ≤ 256-row chunks.
        let (region, span) = (FreqRange::new(100e6, 100.1e6), span_h(240));
        let mut releases = 0;
        let (tiles, _) = svc
            .query_tiles(region, span, &mut || {
                assert!(history.try_lock().is_ok(), "ingest locked out mid-report");
                releases += 1;
            })
            .unwrap();
        let g = tiles.grid();
        assert_eq!(g.nt, 960);
        assert!(releases >= 3, "{releases} lock releases");
        assert!(g.nt * g.nf <= REPORT_MAX_CELLS);
        // The stitched grid is the one-shot grid.
        let whole = HistoryTiles::query_level(&history.lock().unwrap(), region, span, 2).unwrap();
        assert_eq!(
            (whole.nt, whole.nf, whole.t_first_cell),
            (g.nt, g.nf, g.t_first_cell)
        );
        // The build itself succeeds on the same grid (an empty store still discloses coverage).
        let r = svc.report(region, span, SiteKey::Unassigned).unwrap();
        assert!(r.occupancy.bands.iter().all(|s| s.fco.is_none()));
    }
}
