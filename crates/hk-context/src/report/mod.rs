//! Survey reports (T-121, ADR-0012 §6): `report(region, span)` assembled from the occupancy series,
//! inventory, baselines, observation log and history provenance; JSON/CSV/PNG export.
//!
//! The assembler ([`assemble`]) reads every section through a small provider trait so the pieces
//! that land in other tasks plug in without touching it:
//!
//! | Section | Trait | Provider today | Adapter slot |
//! |---|---|---|---|
//! | coverage + POI (§5.5, §6.2) | [`CoverageProvider`] | [`ObservationCoverage`] (T-115 log), then [`HistoryTiles`] when the log has nothing for the box | — |
//! | occupancy (§2) | [`OccupancyProvider`] | [`HistoryTiles`]: FCO/FBO from tile occupancy over blind-learned (inventory) extents | T-118: `OccupancyStat` series + learned channel plan |
//! | top emitters, anomalies (C27) | [`InventoryProvider`] | [`RepoInventory`] | — |
//! | change vs baseline (§3) | [`BaselineProvider`] | [`NoBaselines`]: `unavailable` | T-119: baseline comparison |
//! | provenance steps (§6.3) | [`ProvenanceProvider`] | [`HistoryTiles`] (`ProvenanceSummary`) | T-115 source restarts/site changes |
//!
//! **Coverage is mandatory.** A report whose coverage providers all decline fails
//! ([`ReportError::NoCoverage`]); every report passes [`SurveyReport::validate`] before it is
//! returned, and its statement always says unobserved is not quiet. Channels come from blind
//! detections (inventory extents), never from a band plan; explanation labels are suggestions.
//! All times are sample-clock times (ADR-0012 §0): `generated_at` is supplied by the caller from
//! the stores' stream time, never the wall clock.

mod export;
mod history;
mod inventory;
mod observation;

use hk_model::attention::ATTENTION_SCHEMA_VERSION;
use hk_model::attention::ValidationError;
use hk_model::attention::baseline::SiteKey;
use hk_model::attention::occupancy::OccupancyStat;
use hk_model::attention::report::{
    BaselineComparison, ComparisonStatus, CoverageDisclosure, CoverageGap, ProvenanceStep,
    ReportEmitter, ReportOccupancy, SurveyReport,
};
use hk_model::attention::schedule::{PoiEntry, poi_fraction};
use hk_model::{AnomalyId, FreqRange, TimeRange, Timestamp};

pub use export::{report_csv, report_png};
pub use history::{HistoryTiles, REPORT_MAX_FREQ_CELLS, REPORT_MAX_TIME_CELLS, steps_from_summary};
pub use inventory::{EXPLANATIONS_AUTHOR_REF, RepoInventory};
pub use observation::ObservationCoverage;

/// Burst durations every report discloses POI for, s (§5.5).
pub const REPORT_POI_TAUS_S: [f64; 4] = [0.005, 0.1, 1.0, 10.0];
/// Frequency cells the coverage computation splits a region into (at most).
pub const MAX_COVERAGE_CELLS: usize = 64;
/// Coverage gaps listed (longest first).
pub const MAX_REPORT_GAPS: usize = 64;
/// Default channel rows.
pub const DEFAULT_MAX_CHANNELS: usize = 64;
/// Default top-emitter rows.
pub const DEFAULT_MAX_EMITTERS: usize = 20;

/// What to report on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReportRequest {
    /// Region.
    pub region: FreqRange,
    /// Span.
    pub span: TimeRange,
    /// Site the report is for.
    pub site: SiteKey,
    /// Generation time (sample clock: the stores' stream time).
    pub generated_at: Timestamp,
    /// Channel rows kept.
    pub max_channels: usize,
    /// Top-emitter rows kept.
    pub max_emitters: usize,
}

impl ReportRequest {
    /// A request with default caps and an unassigned site.
    pub fn new(region: FreqRange, span: TimeRange, generated_at: Timestamp) -> Self {
        Self {
            region,
            span,
            site: SiteKey::Unassigned,
            generated_at,
            max_channels: DEFAULT_MAX_CHANNELS,
            max_emitters: DEFAULT_MAX_EMITTERS,
        }
    }
}

/// Why a report could not be built.
#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    /// Bad region/span.
    #[error("invalid request: {0}")]
    Invalid(&'static str),
    /// No provider could disclose coverage: a report without coverage is refused (§6.2).
    #[error("no coverage source for this region and span")]
    NoCoverage,
    /// A provider's store failed.
    #[error("{0}")]
    Provider(String),
    /// The assembled report failed validation.
    #[error("report validation: {0}")]
    Validation(#[from] ValidationError),
}

/// Observed intervals per frequency cell (§5.5 input).
pub trait CoverageProvider {
    /// Short source name for the report's warnings.
    fn name(&self) -> &'static str;
    /// For each of `cells`, the intervals in which the whole cell was observed (any order), or
    /// `None` when this source knows nothing about the box (the next provider is asked).
    fn observed(
        &self,
        cells: &[FreqRange],
        span: TimeRange,
    ) -> Result<Option<Vec<Vec<TimeRange>>>, ReportError>;
}

/// Occupancy rows from a provider, before sorting and capping.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OccupancyRows {
    /// Band rows.
    pub bands: Vec<OccupancyStat>,
    /// Channel rows with the extent they were computed over.
    pub channels: Vec<(FreqRange, OccupancyStat)>,
    /// Notes (e.g. the interim source).
    pub warnings: Vec<String>,
}

/// Occupancy per channel/band (C12).
pub trait OccupancyProvider {
    /// Rows for the region's band and for `channels` (blind-learned extents inside the region).
    fn occupancy(
        &self,
        req: &ReportRequest,
        channels: &[FreqRange],
    ) -> Result<OccupancyRows, ReportError>;
}

/// Inventory (C27): top emitters and anomalies overlapping the box.
pub trait InventoryProvider {
    /// Emitters seen in the box, most sightings first (uncapped; the assembler caps).
    fn emitters(&self, req: &ReportRequest) -> Result<Vec<ReportEmitter>, ReportError>;
    /// Anomalies overlapping the box.
    fn anomalies(&self, req: &ReportRequest) -> Result<Vec<AnomalyId>, ReportError>;
}

/// Change vs baseline (C12, T-119).
pub trait BaselineProvider {
    /// The comparison for the report's occupancy.
    fn compare(
        &self,
        req: &ReportRequest,
        occupancy: &ReportOccupancy,
    ) -> Result<BaselineComparison, ReportError>;
}

/// No baselines on this server (before T-119): always `unavailable`, never a silent "no change".
#[derive(Clone, Copy, Debug, Default)]
pub struct NoBaselines;

impl BaselineProvider for NoBaselines {
    fn compare(
        &self,
        _req: &ReportRequest,
        _occupancy: &ReportOccupancy,
    ) -> Result<BaselineComparison, ReportError> {
        Ok(BaselineComparison {
            status: ComparisonStatus::Unavailable,
            baseline: None,
            resolution: None,
            changes: Vec::new(),
        })
    }
}

/// Front-end provenance steps (§6.3).
pub trait ProvenanceProvider {
    /// Steps inside the box (any order) and warnings (mixed calibration, overload share…).
    fn steps(&self, req: &ReportRequest)
    -> Result<(Vec<ProvenanceStep>, Vec<String>), ReportError>;
}

/// The providers one report reads.
pub struct Providers<'a> {
    /// Coverage sources, most authoritative first.
    pub coverage: Vec<&'a dyn CoverageProvider>,
    /// Occupancy.
    pub occupancy: &'a dyn OccupancyProvider,
    /// Inventory.
    pub inventory: &'a dyn InventoryProvider,
    /// Baselines.
    pub baseline: &'a dyn BaselineProvider,
    /// Provenance.
    pub provenance: &'a dyn ProvenanceProvider,
}

/// Builds and validates `report(region, span)`.
pub fn assemble(req: &ReportRequest, p: &Providers<'_>) -> Result<SurveyReport, ReportError> {
    if !(req.region.lo_hz.is_finite() && req.region.hi_hz > req.region.lo_hz) {
        return Err(ReportError::Invalid("region must be a positive extent"));
    }
    if req.span.end <= req.span.start {
        return Err(ReportError::Invalid("span must be positive"));
    }
    let mut warnings = Vec::new();

    let cells = coverage_cells(req.region, MAX_COVERAGE_CELLS);
    let mut coverage = None;
    for c in &p.coverage {
        if let Some(observed) = c.observed(&cells, req.span)? {
            warnings.push(format!("coverage and POI from {}", c.name()));
            coverage = Some(disclose(
                req.region,
                req.span,
                &cells,
                &observed,
                &REPORT_POI_TAUS_S,
            ));
            break;
        }
    }
    let coverage = coverage.ok_or(ReportError::NoCoverage)?;

    let mut emitters = p.inventory.emitters(req)?;
    let channels = channel_extents(&emitters, req.region);
    let rows = p.occupancy.occupancy(req, &channels)?;
    warnings.extend(rows.warnings);
    for e in &mut emitters {
        let centre = e.freq.center_hz();
        e.fco = rows
            .channels
            .iter()
            .find(|(f, _)| f.lo_hz <= centre && centre < f.hi_hz)
            .and_then(|(_, s)| s.fco);
    }
    emitters.truncate(req.max_emitters);
    let mut channel_rows: Vec<OccupancyStat> = rows.channels.into_iter().map(|(_, s)| s).collect();
    channel_rows.sort_by(|a, b| {
        b.fco
            .unwrap_or(-1.0)
            .total_cmp(&a.fco.unwrap_or(-1.0))
            .then(b.observed_s.total_cmp(&a.observed_s))
    });
    let truncated = channel_rows.len() > req.max_channels;
    channel_rows.truncate(req.max_channels);
    let occupancy = ReportOccupancy {
        bands: rows.bands,
        channels: channel_rows,
        truncated,
    };

    let change_vs_baseline = p.baseline.compare(req, &occupancy)?;
    if change_vs_baseline.status != ComparisonStatus::Available {
        warnings.push(format!(
            "change vs baseline {}: no comparison is implied",
            match change_vs_baseline.status {
                ComparisonStatus::Immature => "immature",
                ComparisonStatus::NoBaseline => "has no baseline",
                _ => "unavailable (baselines are not built on this server)",
            }
        ));
    }
    let (mut provenance_steps, prov_warnings) = p.provenance.steps(req)?;
    warnings.extend(prov_warnings);
    provenance_steps.sort_by_key(|s| s.t);
    let report = SurveyReport {
        schema: ATTENTION_SCHEMA_VERSION,
        generated_at: req.generated_at,
        region: req.region,
        span: req.span,
        site: req.site,
        occupancy,
        top_emitters: emitters,
        change_vs_baseline,
        coverage,
        provenance_steps,
        anomalies: p.inventory.anomalies(req)?,
        warnings,
    };
    report.validate()?;
    Ok(report)
}

/// `region` split into at most `max` equal cells.
pub fn coverage_cells(region: FreqRange, max: usize) -> Vec<FreqRange> {
    let n = max.max(1);
    let w = region.width_hz() / n as f64;
    (0..n)
        .map(|k| {
            let lo = region.lo_hz + k as f64 * w;
            let hi = if k + 1 == n { region.hi_hz } else { lo + w };
            FreqRange::new(lo, hi)
        })
        .collect()
}

/// Blind channel extents: emitter extents clipped to the region, overlapping ones merged.
fn channel_extents(emitters: &[ReportEmitter], region: FreqRange) -> Vec<FreqRange> {
    let mut v: Vec<FreqRange> = emitters
        .iter()
        .map(|e| {
            FreqRange::new(
                e.freq.lo_hz.max(region.lo_hz),
                e.freq.hi_hz.min(region.hi_hz),
            )
        })
        .filter(|f| f.hi_hz > f.lo_hz)
        .collect();
    v.sort_by(|a, b| a.lo_hz.total_cmp(&b.lo_hz));
    let mut out: Vec<FreqRange> = Vec::with_capacity(v.len());
    for f in v {
        match out.last_mut() {
            Some(last) if f.lo_hz < last.hi_hz => last.hi_hz = last.hi_hz.max(f.hi_hz),
            _ => out.push(f),
        }
    }
    out
}

/// Merged, span-clipped intervals as `(start, end)` ns.
fn merged(intervals: &[TimeRange], span: TimeRange) -> Vec<(i64, i64)> {
    let (s0, s1) = (span.start.as_unix_nanos(), span.end.as_unix_nanos());
    let mut v: Vec<(i64, i64)> = intervals
        .iter()
        .map(|r| {
            (
                r.start.as_unix_nanos().max(s0),
                r.end.as_unix_nanos().min(s1),
            )
        })
        .filter(|(a, b)| b > a)
        .collect();
    v.sort_unstable();
    let mut out: Vec<(i64, i64)> = Vec::with_capacity(v.len());
    for (a, b) in v {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// The coverage disclosure (§5.5, §6.2) of `region` over `span` from per-cell observed intervals:
/// frequency-weighted observed fraction and seconds, gaps longer than twice the measured mean
/// revisit (coalesced across adjacent cells with the same gap, longest first), never-observed
/// ranges, frequency-weighted POI per τ, and the statement.
pub fn disclose(
    region: FreqRange,
    span: TimeRange,
    cells: &[FreqRange],
    observed: &[Vec<TimeRange>],
    taus_s: &[f64],
) -> CoverageDisclosure {
    let span_ns = span.duration_ns().max(1);
    let width = region.width_hz().max(f64::MIN_POSITIVE);
    let empty = Vec::new();
    let per_cell: Vec<Vec<(i64, i64)>> = (0..cells.len())
        .map(|k| merged(observed.get(k).unwrap_or(&empty), span))
        .collect();
    let (mut fraction, mut observed_s) = (0.0, 0.0);
    let (mut revisit_sum, mut revisit_n) = (0.0, 0usize);
    let mut poi = vec![0.0; taus_s.len()];
    for (k, cell) in cells.iter().enumerate() {
        let w = cell.width_hz() / width;
        let obs_ns: i64 = per_cell[k].iter().map(|(a, b)| b - a).sum();
        fraction += w * obs_ns as f64 / span_ns as f64;
        observed_s += w * obs_ns as f64 / 1e9;
        let m = &per_cell[k];
        if m.len() >= 2 {
            revisit_sum += (m[m.len() - 1].0 - m[0].0) as f64 / 1e9 / (m.len() - 1) as f64;
            revisit_n += 1;
        }
        let raw = observed.get(k).unwrap_or(&empty);
        for (i, tau) in taus_s.iter().enumerate() {
            poi[i] += w * poi_fraction(raw, span, (tau * 1e9) as i64);
        }
    }
    let span_s = span_ns as f64 / 1e9;
    let threshold_s = 2.0
        * if revisit_n > 0 {
            revisit_sum / revisit_n as f64
        } else {
            span_s / 4.0
        };
    let threshold_ns = (threshold_s * 1e9) as i64;
    let (s0, s1) = (span.start.as_unix_nanos(), span.end.as_unix_nanos());
    let mut gaps: Vec<CoverageGap> = Vec::new();
    let mut open: Vec<(usize, i64, i64)> = Vec::new();
    let mut never: Vec<FreqRange> = Vec::new();
    for (k, cell) in cells.iter().enumerate() {
        let m = &per_cell[k];
        if m.is_empty() {
            match never.last_mut() {
                Some(last) if (last.hi_hz - cell.lo_hz).abs() < 1e-6 => last.hi_hz = cell.hi_hz,
                _ => never.push(*cell),
            }
        }
        let mut complement = Vec::new();
        let mut cursor = s0;
        for &(a, b) in m {
            if a > cursor {
                complement.push((cursor, a));
            }
            cursor = cursor.max(b);
        }
        if s1 > cursor {
            complement.push((cursor, s1));
        }
        let mut next_open = Vec::new();
        for (a, b) in complement.into_iter().filter(|(a, b)| b - a > threshold_ns) {
            if let Some(&(gi, _, _)) = open.iter().find(|(_, oa, ob)| *oa == a && *ob == b) {
                gaps[gi].freq.hi_hz = cell.hi_hz;
                next_open.push((gi, a, b));
            } else {
                gaps.push(CoverageGap {
                    freq: *cell,
                    time: TimeRange::new(
                        Timestamp::from_unix_nanos(a),
                        Timestamp::from_unix_nanos(b),
                    ),
                });
                next_open.push((gaps.len() - 1, a, b));
            }
        }
        open = next_open;
    }
    gaps.sort_by(|a, b| {
        b.time
            .duration_ns()
            .cmp(&a.time.duration_ns())
            .then(a.freq.lo_hz.total_cmp(&b.freq.lo_hz))
    });
    let gaps_truncated = gaps.len() > MAX_REPORT_GAPS;
    gaps.truncate(MAX_REPORT_GAPS);
    let fraction = fraction.clamp(0.0, 1.0);
    let poi: Vec<PoiEntry> = taus_s
        .iter()
        .zip(poi)
        .map(|(&tau_s, p)| PoiEntry {
            tau_s,
            p_poi: p.clamp(0.0, 1.0),
            rate_hz: None,
            p_at_least_one: None,
        })
        .collect();
    let statement = coverage_statement(fraction, span_s, gaps.len(), &never, &poi, threshold_s);
    CoverageDisclosure {
        observed_fraction: fraction,
        observed_s,
        gaps,
        gaps_truncated,
        never_observed: never,
        poi,
        statement,
    }
}

fn coverage_statement(
    fraction: f64,
    span_s: f64,
    gaps: usize,
    never: &[FreqRange],
    poi: &[PoiEntry],
    threshold_s: f64,
) -> String {
    let poi_text: Vec<String> = poi
        .iter()
        .map(|p| format!("{} s burst {:.3}", p.tau_s, p.p_poi))
        .collect();
    format!(
        "{:.2} % of this region over {:.0} s was observed; {} coverage gap(s) longer than {:.0} s \
         and {} never-observed range(s). Unobserved is not quiet: nothing is claimed about \
         activity where or when the radio did not look. Probability of intercept: {}.",
        fraction * 100.0,
        span_s,
        gaps,
        threshold_s,
        never.len(),
        poi_text.join(", ")
    )
}

#[cfg(test)]
mod tests;
