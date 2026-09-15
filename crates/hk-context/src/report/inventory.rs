//! Inventory provider (C27): top emitters found blind, with their top explanation as a suggestion.

use hk_model::attention::report::ReportEmitter;
use hk_model::{
    AnnotationAuthor, AnnotationTarget, AnomalyId, EmitterId, InventoryQuery, Region, RepoError,
    Repository,
};

use super::{InventoryProvider, ReportError, ReportRequest};

/// `author_ref` of the Classifier annotations holding ranked explanations: must equal
/// `hk_pipeline::family::FAMILY_MAP_VERSION` (as `hk_api::query::EXPLANATIONS_AUTHOR_REF` does;
/// hk-context does not depend on hk-pipeline).
pub const EXPLANATIONS_AUTHOR_REF: &str = "hk-pipeline/family-map@1";

/// Inventory rows read per report.
pub const MAX_REPORT_INVENTORY_ROWS: u32 = 500;

/// Appearances read per emitter to count sightings inside the span (newest first).
pub const REPORT_APPEARANCES: usize = 512;

/// [`InventoryProvider`] over the inventory database.
pub struct RepoInventory<'a>(pub &'a Repository);

fn failed(e: RepoError) -> ReportError {
    ReportError::Provider(format!("inventory query failed: {e}"))
}

impl RepoInventory<'_> {
    /// The top-ranked explanation's service label, a suggestion (never truth).
    fn top_suggestion(&self, id: EmitterId) -> Result<Option<String>, ReportError> {
        Ok(self
            .0
            .annotations_for(&AnnotationTarget::Emitter(id))
            .map_err(failed)?
            .into_iter()
            .rfind(|a| {
                a.author == AnnotationAuthor::Classifier
                    && a.author_ref == EXPLANATIONS_AUTHOR_REF
                    && a.content.is_none()
            })
            .and_then(|a| {
                a.metadata["explanations"][0]["service"]
                    .as_str()
                    .map(str::to_owned)
            }))
    }
}

impl InventoryProvider for RepoInventory<'_> {
    fn emitters(&self, req: &ReportRequest) -> Result<Vec<ReportEmitter>, ReportError> {
        let page = self
            .0
            .query_inventory(&InventoryQuery {
                freq: Some(req.region),
                time: Some(req.span),
                limit: MAX_REPORT_INVENTORY_ROWS,
                ..InventoryQuery::default()
            })
            .map_err(failed)?;
        let mut out = Vec::with_capacity(page.entries.len());
        for entry in page.entries {
            let e = &entry.emitter;
            let rec = self
                .0
                .emitter_recurrence(e.id, REPORT_APPEARANCES)
                .map_err(failed)?;
            let sightings = if rec.recent.is_empty() {
                e.count
            } else {
                rec.recent
                    .iter()
                    .filter(|a| a.time.overlaps(&req.span))
                    .map(|a| a.count)
                    .sum()
            };
            let lifecycle = serde_json::to_value(entry.lifecycle)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            out.push(ReportEmitter {
                emitter_id: e.id,
                freq: e.freq(),
                first_seen: e.first_seen,
                last_seen: e.last_seen,
                sightings,
                lifecycle,
                fco: None,
                fco_all_visits: None,
                top_suggestion: self.top_suggestion(e.id)?,
                new_in_span: e.first_seen >= req.span.start && e.first_seen <= req.span.end,
            });
        }
        out.sort_by(|a, b| {
            b.sightings
                .cmp(&a.sightings)
                .then(a.freq.lo_hz.total_cmp(&b.freq.lo_hz))
        });
        Ok(out)
    }

    fn anomalies(&self, req: &ReportRequest) -> Result<Vec<AnomalyId>, ReportError> {
        Ok(self
            .0
            .anomalies_in_region(&Region::new(req.region, req.span))
            .map_err(failed)?
            .into_iter()
            .map(|a| a.id)
            .collect())
    }
}
