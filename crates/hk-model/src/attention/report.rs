//! Survey report schema (ADR-0012 §6): `report(region, span)`.
//!
//! Coverage and POI are **always** disclosed (a report never implies "nothing there" where the
//! radio did not look); the baseline comparison is explicit about being unavailable.

use serde::{Deserialize, Serialize};

use super::alarm::AlarmKind;
use super::baseline::{BaselineKey, BaselineResolution, SiteKey};
use super::occupancy::{OccupancyStat, OccupancySubject};
use super::schedule::PoiEntry;
use super::{ValidationError, ensure, ensure_in, ensure_opt_in, ensure_schema};
use crate::ids::{AnomalyId, EmitterId};
use crate::region::{FreqRange, TimeRange};
use crate::time::Timestamp;

/// Occupancy section.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportOccupancy {
    /// Band rows (FBO, SRO).
    pub bands: Vec<OccupancyStat>,
    /// Channel rows (FCO), highest FCO first.
    pub channels: Vec<OccupancyStat>,
    /// Channel rows were capped.
    pub truncated: bool,
}

/// One top-emitter row, from the inventory (C27).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportEmitter {
    /// Emitter.
    pub emitter_id: EmitterId,
    /// Measured extent.
    pub freq: FreqRange,
    /// First seen (ever).
    pub first_seen: Timestamp,
    /// Last seen.
    pub last_seen: Timestamp,
    /// Sightings inside the span.
    pub sightings: u64,
    /// Lifecycle state (candidate/confirmed), as the inventory names it.
    pub lifecycle: String,
    /// Unbiased FCO of its channel over the span (activity-independent visits only, ADR-0012
    /// §2.5); `None` when the occupancy provider cannot give it (never substituted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fco: Option<f64>,
    /// Its channel's all-visits FCO (`OccupancyStat::fco_all_visits`), for information.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fco_all_visits: Option<f64>,
    /// Top-ranked explanation label (a suggestion, never truth).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_suggestion: Option<String>,
    /// First seen inside the span.
    pub new_in_span: bool,
}

/// Whether a baseline comparison could be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ComparisonStatus {
    /// Compared.
    Available,
    /// A baseline exists but no pool is mature.
    Immature,
    /// No baseline for this site/calibration (or the site is mobile/unassigned).
    NoBaseline,
    /// Baselines are not built on this server (before T-119).
    Unavailable,
}

/// The alarm kinds a report change vs baseline can carry (T-131: the report uses [`AlarmKind`]
/// itself, so a report change and a live alarm on the same subject share one name).
pub const REPORT_CHANGE_KINDS: [AlarmKind; 3] = [
    AlarmKind::LevelAboveBaseline,
    AlarmKind::BusierThanUsual,
    AlarmKind::QuieterThanUsual,
];

/// One change vs baseline.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeEntry {
    /// Channel or band.
    pub subject: OccupancySubject,
    /// Change kind: one of [`REPORT_CHANGE_KINDS`] (`level-above-baseline`, `busier-than-usual`,
    /// `quieter-than-usual`). New emitters and change points are alarms, not report changes.
    pub kind: AlarmKind,
    /// Baseline value (dB or fraction by kind).
    pub baseline: f64,
    /// Observed value.
    pub observed: f64,
    /// z-score.
    pub z: f64,
}

/// Change-vs-baseline section.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaselineComparison {
    /// Status.
    pub status: ComparisonStatus,
    /// Baseline compared against (`available` and `immature`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<BaselineKey>,
    /// Pool used (`available`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<BaselineResolution>,
    /// Changes, largest |z| first.
    pub changes: Vec<ChangeEntry>,
}

/// An unobserved frequency × time box inside the report region.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageGap {
    /// Extent.
    pub freq: FreqRange,
    /// When.
    pub time: TimeRange,
}

/// Coverage disclosure (§6.2). Mandatory in every report.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageDisclosure {
    /// Observed share of the region × span (frequency-weighted time), 0–1.
    pub observed_fraction: f64,
    /// Observed seconds summed over the region's frequency-weighted extent.
    pub observed_s: f64,
    /// Gaps longer than the gap threshold, longest first.
    pub gaps: Vec<CoverageGap>,
    /// Gap list was capped.
    pub gaps_truncated: bool,
    /// Sub-ranges never observed in the span.
    pub never_observed: Vec<FreqRange>,
    /// POI rows for the standard burst durations (5 ms, 100 ms, 1 s, 10 s).
    pub poi: Vec<PoiEntry>,
    /// Plain-language statement, always including "unobserved is not quiet" when
    /// `observed_fraction < 1`.
    pub statement: String,
}

/// Kind of front-end/provenance step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProvenanceStepKind {
    /// LNA/VGA/amp change.
    Gain,
    /// Calibration state change.
    Calibration,
    /// Spur mask version change.
    SpurMask,
    /// Antenna/filter port change.
    AntennaPort,
    /// Overload began or ended.
    Overload,
    /// Source reopened/restarted.
    SourceRestart,
    /// Samples dropped.
    SampleDrop,
    /// Site changed (moved).
    Site,
}

/// A provenance step in the span (§6.3): what C30 rules out first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvenanceStep {
    /// When.
    pub t: Timestamp,
    /// Kind.
    pub kind: ProvenanceStepKind,
    /// Affected extent, when narrower than the region.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freq: Option<FreqRange>,
    /// Short description, e.g. `lna 32→24 dB`.
    pub detail: String,
}

/// Export format (§6.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExportFormat {
    /// The report document.
    Json,
    /// Channel occupancy rows plus coverage gaps.
    Csv,
    /// Occupancy/history heatmap rendered by the backend.
    Png,
}

/// A survey report (§6.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurveyReport {
    /// [`super::ATTENTION_SCHEMA_VERSION`].
    pub schema: u32,
    /// Generation time.
    pub generated_at: Timestamp,
    /// Region.
    pub region: FreqRange,
    /// Span.
    pub span: TimeRange,
    /// Site.
    pub site: SiteKey,
    /// Occupancy.
    pub occupancy: ReportOccupancy,
    /// Top emitters.
    pub top_emitters: Vec<ReportEmitter>,
    /// Change vs baseline.
    pub change_vs_baseline: BaselineComparison,
    /// Coverage and POI.
    pub coverage: CoverageDisclosure,
    /// Provenance steps in the span, time order.
    pub provenance_steps: Vec<ProvenanceStep>,
    /// Anomalies overlapping the region × span.
    pub anomalies: Vec<AnomalyId>,
    /// Warnings (e.g. calibration mixed, mobile periods).
    pub warnings: Vec<String>,
}

impl SurveyReport {
    /// Checks schema, extents, coverage disclosure and section consistency.
    pub fn validate(&self) -> Result<(), ValidationError> {
        ensure_schema(self.schema)?;
        ensure(
            self.region.hi_hz > self.region.lo_hz,
            "region",
            "must be a positive extent",
        )?;
        ensure(self.span.end > self.span.start, "span", "must be positive")?;
        let c = &self.coverage;
        ensure_in(c.observed_fraction, 0.0, 1.0, "coverage.observed_fraction")?;
        ensure_in(c.observed_s, 0.0, f64::MAX, "coverage.observed_s")?;
        ensure(
            !c.poi.is_empty(),
            "coverage.poi",
            "POI must always be disclosed",
        )?;
        c.poi.iter().try_for_each(PoiEntry::validate)?;
        ensure(
            !c.statement.trim().is_empty(),
            "coverage.statement",
            "must not be blank",
        )?;
        ensure(
            c.observed_fraction >= 1.0 || c.statement.contains("not quiet"),
            "coverage.statement",
            "partial coverage must say unobserved is not quiet",
        )?;
        for g in &c.gaps {
            ensure(
                g.freq.overlaps(&self.region) && g.time.overlaps(&self.span),
                "coverage.gaps",
                "must lie in the report box",
            )?;
        }
        let b = &self.change_vs_baseline;
        match b.status {
            ComparisonStatus::Available => ensure(
                b.baseline.is_some() && b.resolution.is_some(),
                "change_vs_baseline",
                "available needs baseline and resolution",
            )?,
            _ => ensure(
                b.changes.is_empty(),
                "change_vs_baseline.changes",
                "only when available",
            )?,
        }
        for c in &b.changes {
            ensure(
                REPORT_CHANGE_KINDS.contains(&c.kind),
                "change_vs_baseline.changes.kind",
                "level-above-baseline, busier-than-usual or quieter-than-usual",
            )?;
        }
        for s in self.occupancy.bands.iter().chain(&self.occupancy.channels) {
            s.validate()?;
        }
        for e in &self.top_emitters {
            ensure_opt_in(e.fco, 0.0, 1.0, "top_emitters.fco")?;
            ensure_opt_in(e.fco_all_visits, 0.0, 1.0, "top_emitters.fco_all_visits")?;
        }
        ensure(
            self.provenance_steps.windows(2).all(|w| w[0].t <= w[1].t),
            "provenance_steps",
            "must be in time order",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attention::schedule::nominal_poi;

    fn report() -> SurveyReport {
        let t = |s: i64| Timestamp::from_unix_nanos(s * 1_000_000_000);
        SurveyReport {
            schema: 1,
            generated_at: t(7200),
            region: FreqRange::new(430e6, 440e6),
            span: TimeRange::new(t(0), t(3600)),
            site: SiteKey::Unassigned,
            occupancy: ReportOccupancy {
                bands: vec![],
                channels: vec![],
                truncated: false,
            },
            top_emitters: vec![],
            change_vs_baseline: BaselineComparison {
                status: ComparisonStatus::NoBaseline,
                baseline: None,
                resolution: None,
                changes: vec![],
            },
            coverage: CoverageDisclosure {
                observed_fraction: 0.4,
                observed_s: 1440.0,
                gaps: vec![CoverageGap {
                    freq: FreqRange::new(430e6, 432e6),
                    time: TimeRange::new(t(10), t(600)),
                }],
                gaps_truncated: false,
                never_observed: vec![],
                poi: vec![PoiEntry {
                    tau_s: 0.005,
                    p_poi: nominal_poi(0.005, 0.4, 750.0),
                    rate_hz: None,
                    p_at_least_one: None,
                }],
                statement: "40 % observed; unobserved is not quiet".into(),
            },
            provenance_steps: vec![],
            anomalies: vec![],
            warnings: vec![],
        }
    }

    #[test]
    fn coverage_is_always_disclosed() {
        let r = report();
        r.validate().unwrap();
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<SurveyReport>(&json).unwrap(), r);

        let mut no_poi = r.clone();
        no_poi.coverage.poi.clear();
        assert_eq!(no_poi.validate().unwrap_err().field, "coverage.poi");

        let mut silent = r.clone();
        silent.coverage.statement = "40 % observed".into();
        assert_eq!(silent.validate().unwrap_err().field, "coverage.statement");

        let mut v = serde_json::to_value(&r).unwrap();
        v.as_object_mut().unwrap().remove("coverage");
        assert!(
            serde_json::from_value::<SurveyReport>(v).is_err(),
            "coverage is not optional"
        );
    }

    #[test]
    fn baseline_comparison_is_explicit() {
        let mut r = report();
        r.change_vs_baseline.status = ComparisonStatus::Available;
        assert_eq!(r.validate().unwrap_err().field, "change_vs_baseline");
        let mut r = report();
        r.change_vs_baseline.changes.push(ChangeEntry {
            subject: OccupancySubject::Band { freq: r.region },
            kind: AlarmKind::BusierThanUsual,
            baseline: 0.1,
            observed: 0.5,
            z: 9.0,
        });
        assert_eq!(
            r.validate().unwrap_err().field,
            "change_vs_baseline.changes"
        );
    }

    /// T-131: report changes carry the alarm kind, on the wire by the alarm's name; alarm-only
    /// kinds are refused.
    #[test]
    fn report_change_kind_is_the_alarm_kind() {
        let e = ChangeEntry {
            subject: OccupancySubject::Band {
                freq: FreqRange::new(433.0e6, 434.0e6),
            },
            kind: AlarmKind::QuieterThanUsual,
            baseline: 0.5,
            observed: 0.0,
            z: -6.0,
        };
        let v = serde_json::to_value(e).unwrap();
        assert_eq!(v["kind"], "quieter-than-usual");
        let back: ChangeEntry = serde_json::from_value(v).unwrap();
        assert_eq!(back.kind, AlarmKind::QuieterThanUsual);
        let mut r = report();
        r.change_vs_baseline = BaselineComparison {
            status: ComparisonStatus::Available,
            baseline: Some(crate::attention::baseline::BaselineKey {
                site: crate::ids::SiteId::new(),
                cal: crate::attention::baseline::CalKey::Uncalibrated,
                scheme: 1,
                cell_factor: 16,
            }),
            resolution: Some(BaselineResolution::AllHours),
            changes: vec![e],
        };
        r.validate().unwrap();
        r.change_vs_baseline.changes[0].kind = AlarmKind::ChangePoint;
        assert_eq!(
            r.validate().unwrap_err().field,
            "change_vs_baseline.changes.kind"
        );
    }
}
