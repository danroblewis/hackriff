//! Survey report wiring (T-121, ADR-0012 §6): the API-facing report provider over the stores the
//! running pipeline owns.
//!
//! [`ReportService`] reads the spectrum history (the history store, else the floor product's
//! uncalibrated pyramid, as `/api/history` does), the observation log (T-115) and the inventory
//! database, and assembles `report(region, span)` through `hk_context::report`. Coverage comes
//! from the observation log when it holds visits for the box, else from history tiles. Occupancy
//! is the interim history-tile provider (`fco_all_visits` only, never `fco`) until T-118 lands;
//! baselines are `unavailable` until T-119 lands (swap the providers in [`ReportService::build`],
//! nothing else changes).
//!
//! **Ingest lock.** The history writer only `try_lock`s its store and drops frames while the lock
//! is held, so the report grid is bounded ([`report_chunks`]: ≤ `REPORT_MAX_CELLS`, else 400) and
//! read in chunks of ≤ `REPORT_LOCK_CHUNK_ROWS` rows, each under its own short lock scope. One grid
//! serves JSON, CSV and PNG; assembly runs with the history lock released.

use std::sync::{Arc, Mutex};

pub use hk_context::report::ReportError;
use hk_context::report::{
    self, CoverageProvider, HistoryTiles, NoBaselines, ObservationCoverage, Providers,
    RepoInventory, ReportRequest, report_chunks, report_csv, report_png,
};
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
        }
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
        let r = report::assemble(
            &req,
            &Providers {
                coverage,
                // T-118 adapter slot: the OccupancyStat series (true `fco`) replaces tiles here.
                occupancy: &tiles,
                inventory: &inventory,
                // T-119 adapter slot: the baseline comparison replaces `NoBaselines` here.
                baseline: &NoBaselines,
                provenance: &tiles,
            },
        )?;
        drop(repo);
        Ok((r, tiles))
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
