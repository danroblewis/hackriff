//! Survey report route (T-121, ADR-0012 §6, §8): `GET /api/report?f_lo&f_hi&t0&t1[&site][&format]`.
//!
//! Answers the `SurveyReport` document (`format=json`, the default) or its backend-rendered export
//! (`csv`, `png`). Assembly lives in `hk_context::report` behind [`ReportControl`], implemented
//! over the run's stores by `hk_pipeline::reports::ReportService` (wired in `hk serve`). Every
//! report discloses coverage and POI; a server that cannot disclose coverage answers 404 rather
//! than a report implying "nothing there".
//!
//! Served from the HTTP dispatch ([`serve`]) rather than the JSON-only control chain ([`route`])
//! because exports are not JSON.

use std::str::FromStr;

use hk_model::attention::baseline::SiteKey;
use hk_model::attention::report::{ExportFormat, SurveyReport};
use hk_model::ids::SiteId;
use hk_model::{FreqRange, TimeRange, Timestamp};
use hk_store::history::OriginFilter;
use serde_json::Value;

use crate::control::{CtlRequest, CtlResponse};
use crate::http::ApiState;
use crate::query::{ApiError, Params, parse_origin_filter, parse_region};

/// One report request.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReportQuery {
    /// Region.
    pub region: FreqRange,
    /// Span (sample clock).
    pub span: TimeRange,
    /// Site (`unassigned` when not given): the baseline and occupancy-series key.
    pub site: SiteKey,
    /// T-133: the history source/site filter (`source` and an explicitly given `site`; no filter
    /// when neither is given).
    pub filter: OriginFilter,
}

/// A refused or failed report: HTTP status and message (never echoes values).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportFail {
    /// HTTP status.
    pub status: u16,
    /// Message.
    pub message: String,
}

/// The report service of a running server.
pub trait ReportControl: Send + Sync {
    /// The report document.
    fn report(&self, q: &ReportQuery) -> Result<SurveyReport, ReportFail>;
    /// A CSV or PNG export: `(content type, body)`.
    fn export(
        &self,
        q: &ReportQuery,
        format: ExportFormat,
    ) -> Result<(&'static str, Vec<u8>), ReportFail>;
}

/// What `/api/report` answers.
pub(crate) enum Served {
    /// The report document.
    Json(Value),
    /// An export body.
    Export(&'static str, Vec<u8>),
}

fn param<'a>(q: &'a Params, key: &str) -> Option<&'a str> {
    q.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

fn bad(message: &str) -> ApiError {
    ApiError::new(400, message)
}

/// Parses `site`: `unassigned` (default), `mobile` or a site id; `unknown` (T-133: history of
/// unknown site) keys baselines as `unassigned`.
pub fn parse_site(v: Option<&str>) -> Result<SiteKey, ApiError> {
    match v {
        None | Some("unassigned") | Some("unknown") => Ok(SiteKey::Unassigned),
        Some("mobile") => Ok(SiteKey::Mobile),
        Some(id) => SiteId::from_str(id)
            .map(SiteKey::Site)
            .map_err(|_| bad("site must be unassigned, mobile or a site id")),
    }
}

/// Parses `format`.
pub fn parse_format(v: Option<&str>) -> Result<ExportFormat, ApiError> {
    match v {
        None | Some("json") => Ok(ExportFormat::Json),
        Some("csv") => Ok(ExportFormat::Csv),
        Some("png") => Ok(ExportFormat::Png),
        Some(_) => Err(bad("format must be json, csv or png")),
    }
}

/// `GET /api/report`.
pub(crate) fn serve(state: &ApiState, q: &Params) -> Result<Served, ApiError> {
    let region = parse_region(q)?;
    let query = ReportQuery {
        region: region.freq,
        span: TimeRange::new(
            Timestamp::from_unix_nanos(region.t0_ns),
            Timestamp::from_unix_nanos(region.t1_ns),
        ),
        site: parse_site(param(q, "site"))?,
        filter: parse_origin_filter(q)?,
    };
    let format = parse_format(param(q, "format"))?;
    let Some(ctl) = state.reports.as_ref() else {
        return Err(ApiError::new(503, "no survey reports on this server"));
    };
    let fail = |f: ReportFail| ApiError::new(f.status, f.message);
    match format {
        ExportFormat::Json => {
            let r = ctl.report(&query).map_err(fail)?;
            serde_json::to_value(&r)
                .map(Served::Json)
                .map_err(|_| ApiError::new(500, "report serialisation failed"))
        }
        f => ctl
            .export(&query, f)
            .map(|(ct, body)| Served::Export(ct, body))
            .map_err(fail),
    }
}

/// The control-chain hook: `/api/report` is answered by [`serve`] in the HTTP dispatch.
pub(crate) fn route(_state: &ApiState, _req: &CtlRequest<'_>) -> Option<CtlResponse> {
    None
}
