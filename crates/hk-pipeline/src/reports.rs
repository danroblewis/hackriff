//! Survey report wiring (T-121, ADR-0012 §6): the API-facing report provider over the stores the
//! running pipeline owns.
//!
//! [`ReportService`] reads the spectrum history (the history store, else the floor product's
//! uncalibrated pyramid, as `/api/history` does), the observation log (T-115) and the inventory
//! database, and assembles `report(region, span)` through `hk_context::report`. Coverage comes
//! from the observation log when it holds visits for the box, else from history tiles. Occupancy
//! is the interim history-tile provider until T-118 lands; baselines are `unavailable` until
//! T-119 lands (swap the providers in [`ReportService::build`], nothing else changes).

use std::sync::{Arc, Mutex};

pub use hk_context::report::ReportError;
use hk_context::report::{
    self, CoverageProvider, HistoryTiles, NoBaselines, ObservationCoverage, Providers,
    RepoInventory, ReportRequest, report_csv, report_png,
};
use hk_model::attention::baseline::SiteKey;
use hk_model::attention::report::{ExportFormat, SurveyReport};
use hk_model::{FreqRange, Repository, TimeRange};
use hk_store::observation::ObservationStore;
use hk_store::{FloorProduct, Pyramid};

/// Grid rows of the report's analysis grid (the finest history level that fits is used).
pub const REPORT_TIME_CELLS: usize = hk_context::report::REPORT_MAX_TIME_CELLS;
/// Grid columns of the report's analysis grid.
pub const REPORT_FREQ_CELLS: usize = hk_context::report::REPORT_MAX_FREQ_CELLS;
/// PNG export rows (pixels).
pub const PNG_TIME_CELLS: usize = 2048;
/// PNG export columns (pixels).
pub const PNG_FREQ_CELLS: usize = 1024;

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
        self.build(region, span, site, false).map(|(r, _, _)| r)
    }

    /// The report's export: `(content type, body)`.
    pub fn export(
        &self,
        region: FreqRange,
        span: TimeRange,
        site: SiteKey,
        format: ExportFormat,
    ) -> Result<(&'static str, Vec<u8>), ReportError> {
        let (r, tiles, png) = self.build(region, span, site, format == ExportFormat::Png)?;
        Ok(match format {
            ExportFormat::Json => (
                "application/json",
                serde_json::to_vec(&r).map_err(|e| ReportError::Provider(e.to_string()))?,
            ),
            ExportFormat::Csv => (
                "text/csv; charset=utf-8",
                report_csv(&r, tiles.level0_f_cell_hz()).into_bytes(),
            ),
            ExportFormat::Png => (
                "image/png",
                report_png(png.as_ref().unwrap_or(&tiles).grid()),
            ),
        })
    }

    fn build(
        &self,
        region: FreqRange,
        span: TimeRange,
        site: SiteKey,
        png: bool,
    ) -> Result<(SurveyReport, HistoryTiles, Option<HistoryTiles>), ReportError> {
        if !(region.lo_hz.is_finite() && region.hi_hz > region.lo_hz) || span.end <= span.start {
            return Err(ReportError::Invalid("need f_lo < f_hi and t0 < t1"));
        }
        // Sample clock (ADR-0012 §0): the stream time history has reached, never the wall clock.
        let (tiles, png_tiles, generated_at) = self.with_pyramid(|p| {
            let tiles = HistoryTiles::query(p, region, span, REPORT_TIME_CELLS, REPORT_FREQ_CELLS)?;
            let png_tiles = png
                .then(|| HistoryTiles::query(p, region, span, PNG_TIME_CELLS, PNG_FREQ_CELLS))
                .transpose()?;
            Ok((tiles, png_tiles, p.latest_frame_end().unwrap_or(span.end)))
        })?;
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
                // T-118 adapter slot: the OccupancyStat series replaces the tile provider here.
                occupancy: &tiles,
                inventory: &inventory,
                // T-119 adapter slot: the baseline comparison replaces `NoBaselines` here.
                baseline: &NoBaselines,
                provenance: &tiles,
            },
        )?;
        drop(repo);
        Ok((r, tiles, png_tiles))
    }
}
