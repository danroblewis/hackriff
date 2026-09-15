//! Observation / revisit log (ADR-0012 §1): what the single radio actually observed, when, and
//! why. C04 (scheduler) and the pipeline produce it; C12 (occupancy) weights by it; C26 reports
//! disclose coverage and POI from it.
//!
//! Records describe **observation**, not plans: `observed` is the settled, analysed interval
//! (after retune settle, cut short by preemption), and [`ObservedWindow::usable`] is the analysed
//! frequency extent under the same usable-span rule the history ingest applies, so the log and
//! history tile coverage agree ("not observed" ≠ "quiet").

use serde::{Deserialize, Serialize};

use super::baseline::SiteKey;
use super::{ValidationError, ensure, ensure_in, ensure_schema};
use crate::ids::{ProvenanceId, SurveyId};
use crate::region::{FreqRange, TimeRange};

/// Preemption tier, lowest to highest (ADR-0012 §5.4). `Ord` follows priority: a higher tier
/// preempts a lower one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    /// The continuous discovery sweep.
    BackgroundSweep,
    /// Bandit exploit/explore dwells (and, until T-120, weighted-round-robin POI dwells).
    Bandit,
    /// Scheduled plans: dwell-only regions and plan revisit targets.
    ScheduledPlan,
    /// Pinned leases: user "watch this" pins, decoder/trunking leases, pass/launch windows.
    PinnedLease,
    /// Interactive user intent (live tune, Listen).
    Interactive,
}

impl Tier {
    /// Every tier, highest priority first.
    pub const HIGHEST_FIRST: [Tier; 5] = [
        Tier::Interactive,
        Tier::PinnedLease,
        Tier::ScheduledPlan,
        Tier::Bandit,
        Tier::BackgroundSweep,
    ];

    /// Visits at this tier are chosen independently of measured activity, so occupancy may use
    /// them without revisit (selection) bias (ADR-0012 §2.5).
    pub fn activity_independent(self) -> bool {
        matches!(self, Tier::BackgroundSweep | Tier::ScheduledPlan)
    }
}

/// Kind of pinned lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LeaseKind {
    /// A user "watch this" pin (C39).
    UserPin,
    /// A decoder asked to keep receiving (C22).
    Decoder,
    /// A trunking control channel (C23).
    Trunking,
    /// A satellite pass window (C29/C34).
    Pass,
    /// A time-anchored launch/beacon window, e.g. 00Z/12Z radiosondes (C29).
    Launch,
}

/// Trust test of a verification group (S4 rules 6–7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrustTestKind {
    /// Interleaved gain-step block.
    GainStep,
    /// ±Δ retune dwell.
    Retune,
    /// Sample-rate-change dwell.
    RateChange,
}

/// Why the window pointed where it did: the reason code of a record (ADR-0012 §1.2). `Copy`, so
/// the scheduler can carry it on every step without allocating.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "code", deny_unknown_fields)]
pub enum Reason {
    /// A discovery sweep hop.
    BackgroundSweep {
        /// Hop index in pass order.
        hop: u32,
    },
    /// A window of a scheduled plan's dwell-only region.
    RegionDwell {
        /// Hop index in pass order.
        hop: u32,
    },
    /// A plan region's revisit target fell due.
    RevisitDue {
        /// Plan region index.
        region: u32,
    },
    /// A weighted-round-robin POI dwell (the v1 scheduler; superseded by bandit reasons).
    PoiDwell {
        /// POI key.
        poi: u64,
    },
    /// A verification-group step.
    Verification {
        /// POI key.
        poi: u64,
        /// Which test.
        test: TrustTestKind,
    },
    /// Bandit exploit: the arm's index was led by its interestingness/reward ("novelty 0.8").
    Novelty {
        /// Arm index in the scheduler's arm table.
        arm: u32,
        /// Normalised interestingness of the arm's best candidate, 0–1.
        score: f32,
    },
    /// Bandit exploration: the exploration floor or the UCB bonus chose a stale/unknown arm.
    Explore {
        /// Arm index.
        arm: u32,
    },
    /// A periodic emitter's next burst is due (C10 `next_burst_eta`).
    BeaconDue {
        /// Arm index.
        arm: u32,
        /// Seconds from the step start to the expected burst.
        eta_s: f32,
    },
    /// A pinned lease.
    Lease {
        /// Lease kind ("user pin" is `user-pin`).
        kind: LeaseKind,
        /// Caller's lease id.
        lease: u64,
    },
    /// Interactive user intent.
    Interactive {
        /// Caller's intent id.
        intent: u64,
    },
}

impl Reason {
    /// The preemption tier this reason runs at.
    pub fn tier(&self) -> Tier {
        match self {
            Reason::BackgroundSweep { .. } => Tier::BackgroundSweep,
            Reason::PoiDwell { .. }
            | Reason::Verification { .. }
            | Reason::Novelty { .. }
            | Reason::Explore { .. }
            | Reason::BeaconDue { .. } => Tier::Bandit,
            Reason::RegionDwell { .. } | Reason::RevisitDue { .. } => Tier::ScheduledPlan,
            Reason::Lease { .. } => Tier::PinnedLease,
            Reason::Interactive { .. } => Tier::Interactive,
        }
    }

    /// Short human text for logs and reports, e.g. `novelty 0.80`, `user pin`, `beacon due`,
    /// `background sweep`.
    pub fn text(&self) -> String {
        match self {
            Reason::BackgroundSweep { .. } => "background sweep".into(),
            Reason::RegionDwell { .. } => "scheduled region".into(),
            Reason::RevisitDue { .. } => "revisit due".into(),
            Reason::PoiDwell { .. } => "poi dwell".into(),
            Reason::Verification { test, .. } => match test {
                TrustTestKind::GainStep => "verify gain step".into(),
                TrustTestKind::Retune => "verify retune".into(),
                TrustTestKind::RateChange => "verify rate change".into(),
            },
            Reason::Novelty { score, .. } => format!("novelty {score:.2}"),
            Reason::Explore { .. } => "explore".into(),
            Reason::BeaconDue { .. } => "beacon due".into(),
            Reason::Lease { kind, .. } => match kind {
                LeaseKind::UserPin => "user pin".into(),
                LeaseKind::Decoder => "decoder lease".into(),
                LeaseKind::Trunking => "trunking lease".into(),
                LeaseKind::Pass => "pass window".into(),
                LeaseKind::Launch => "launch window".into(),
            },
            Reason::Interactive { .. } => "interactive".into(),
        }
    }
}

/// The frequency extent one tune actually analysed.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedWindow {
    /// Tuned RF centre, Hz.
    pub center_hz: f64,
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Analysed extent (roll-off edges removed), Hz.
    pub usable: FreqRange,
    /// Excluded DC notch inside `usable`, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dc_excluded: Option<FreqRange>,
    /// Resolution bandwidth of the analysis, Hz.
    pub rbw_hz: f64,
}

impl ObservedWindow {
    /// Observed sub-ranges: `usable` minus `dc_excluded`, ascending.
    pub fn covered(&self) -> Vec<FreqRange> {
        match self.dc_excluded {
            None => vec![self.usable],
            Some(dc) => [
                FreqRange::new(self.usable.lo_hz, dc.lo_hz),
                FreqRange::new(dc.hi_hz, self.usable.hi_hz),
            ]
            .into_iter()
            .filter(|r| r.width_hz() > 0.0)
            .collect(),
        }
    }

    /// Checks geometry: finite, positive widths, usable inside the sampled band, DC notch inside
    /// usable.
    pub fn validate(&self) -> Result<(), ValidationError> {
        ensure_in(
            self.sample_rate_hz,
            f64::MIN_POSITIVE,
            1e12,
            "window.sample_rate_hz",
        )?;
        ensure_in(
            self.rbw_hz,
            f64::MIN_POSITIVE,
            self.sample_rate_hz,
            "window.rbw_hz",
        )?;
        ensure_in(self.center_hz, 0.0, 1e12, "window.center_hz")?;
        let half = self.sample_rate_hz / 2.0;
        ensure(
            self.usable.lo_hz.is_finite()
                && self.usable.hi_hz > self.usable.lo_hz
                && self.usable.lo_hz >= self.center_hz - half
                && self.usable.hi_hz <= self.center_hz + half,
            "window.usable",
            "must be a positive extent inside the sampled band",
        )?;
        if let Some(dc) = self.dc_excluded {
            ensure(
                dc.hi_hz > dc.lo_hz
                    && dc.lo_hz >= self.usable.lo_hz
                    && dc.hi_hz <= self.usable.hi_hz,
                "window.dc_excluded",
                "must be a positive extent inside usable",
            )?;
        }
        Ok(())
    }
}

/// One dwell-class observation (ADR-0012 §1.1): every non-sweep step, one record each.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DwellRecord {
    /// [`super::ATTENTION_SCHEMA_VERSION`].
    pub schema: u32,
    /// Survey the step ran under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub survey_id: Option<SurveyId>,
    /// Scheduler step `seq`.
    pub seq: u64,
    /// ScanPlan version.
    pub plan_version: u32,
    /// Site key at the time.
    pub site: SiteKey,
    /// Why.
    pub reason: Reason,
    /// Tier it ran at; equals `reason.tier()`.
    pub tier: Tier,
    /// Analysed extent.
    pub window: ObservedWindow,
    /// RF path of the tuned centre.
    pub rf_path: u8,
    /// Planned interval.
    pub planned: TimeRange,
    /// Settled, analysed interval (inside `planned`).
    pub observed: TimeRange,
    /// Cut short by a higher tier.
    pub preempted: bool,
    /// Samples dropped (source or ring) inside `observed`.
    pub dropped_samples: u64,
    /// Front end judged overloaded during the dwell.
    pub overload: bool,
    /// Provenance in force, when interned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance_ref: Option<ProvenanceId>,
}

impl DwellRecord {
    /// Observed seconds.
    pub fn observed_s(&self) -> f64 {
        self.observed.duration_ns() as f64 * 1e-9
    }

    /// Checks schema, tier consistency, window geometry and interval nesting.
    pub fn validate(&self) -> Result<(), ValidationError> {
        ensure_schema(self.schema)?;
        ensure(
            self.tier == self.reason.tier(),
            "tier",
            "must equal reason.tier()",
        )?;
        self.window.validate()?;
        ensure(
            self.planned.end >= self.planned.start,
            "planned",
            "end before start",
        )?;
        ensure(
            self.observed.start >= self.planned.start
                && self.observed.end <= self.planned.end
                && self.observed.end >= self.observed.start,
            "observed",
            "must lie inside planned",
        )?;
        if let Reason::Novelty { score, .. } = self.reason {
            ensure_in(f64::from(score), 0.0, 1.0, "reason.score")?;
        }
        Ok(())
    }
}

/// One hop visit inside a [`SweepRecord`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HopVisit {
    /// Index into the referenced [`SweepGeometry::hops`].
    pub hop: u32,
    /// Settled start, ms after the record's `span.start`.
    pub start_ms: u32,
    /// Observed duration, ms (0 when the hop was cut before settling).
    pub observed_ms: u32,
}

/// The hop windows of a discovery pass, written once per plan version / geometry change and
/// referenced by id, so sweep records stay small (ADR-0012 §1.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepGeometry {
    /// [`super::ATTENTION_SCHEMA_VERSION`].
    pub schema: u32,
    /// Id: a hash of the canonical hop list (producer-chosen, stable for equal geometry).
    pub id: u64,
    /// ScanPlan version it was compiled from.
    pub plan_version: u32,
    /// Hop windows in pass order.
    pub hops: Vec<ObservedWindow>,
}

impl SweepGeometry {
    /// Checks schema and every hop window.
    pub fn validate(&self) -> Result<(), ValidationError> {
        ensure_schema(self.schema)?;
        ensure(!self.hops.is_empty(), "hops", "must not be empty")?;
        self.hops.iter().try_for_each(ObservedWindow::validate)
    }
}

/// Discovery-sweep observations aggregated over at most one pass or 60 s, whichever ends first
/// (ADR-0012 §1.3): hops run at ~20 steps/s, so per-hop rows would be ~1.7 M/day.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepRecord {
    /// [`super::ATTENTION_SCHEMA_VERSION`].
    pub schema: u32,
    /// Survey.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub survey_id: Option<SurveyId>,
    /// ScanPlan version.
    pub plan_version: u32,
    /// Site key.
    pub site: SiteKey,
    /// [`SweepGeometry::id`] of the hops.
    pub geometry: u64,
    /// First hop start to last hop end.
    pub span: TimeRange,
    /// Visits in time order.
    pub visits: Vec<HopVisit>,
    /// Hop visits cut by preemption.
    pub preempted_hops: u32,
    /// Samples dropped inside the span.
    pub dropped_samples: u64,
    /// Hops under an overloaded front end.
    pub overload_hops: u32,
}

impl SweepRecord {
    /// Longest span a record may cover, ns.
    pub const MAX_SPAN_NS: i64 = 60_000_000_000;

    /// Checks schema, span bound, time order and that visits lie inside the span.
    pub fn validate(&self) -> Result<(), ValidationError> {
        ensure_schema(self.schema)?;
        let len = self.span.duration_ns();
        ensure(
            (0..=Self::MAX_SPAN_NS).contains(&len),
            "span",
            "must be 0–60 s",
        )?;
        let mut last = 0u32;
        for v in &self.visits {
            ensure(v.start_ms >= last, "visits", "must be in time order")?;
            ensure(
                i64::from(v.start_ms) * 1_000_000 + i64::from(v.observed_ms) * 1_000_000 <= len,
                "visits",
                "must lie inside span",
            )?;
            last = v.start_ms;
        }
        Ok(())
    }
}

/// One line of the observation log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "record")]
pub enum ObservationRecord {
    /// A dwell-class step.
    Dwell(DwellRecord),
    /// Aggregated sweep hops.
    Sweep(SweepRecord),
    /// Hop geometry referenced by later sweep records.
    Geometry(SweepGeometry),
}

impl ObservationRecord {
    /// Validates the inner record.
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            ObservationRecord::Dwell(r) => r.validate(),
            ObservationRecord::Sweep(r) => r.validate(),
            ObservationRecord::Geometry(r) => r.validate(),
        }
    }
}

/// Observed seconds per tier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TierSeconds {
    /// Interactive.
    pub interactive: f64,
    /// Pinned leases.
    pub pinned_lease: f64,
    /// Scheduled plans.
    pub scheduled_plan: f64,
    /// Bandit.
    pub bandit: f64,
    /// Background sweep.
    pub background_sweep: f64,
}

impl TierSeconds {
    /// Adds seconds to a tier.
    pub fn add(&mut self, tier: Tier, s: f64) {
        match tier {
            Tier::Interactive => self.interactive += s,
            Tier::PinnedLease => self.pinned_lease += s,
            Tier::ScheduledPlan => self.scheduled_plan += s,
            Tier::Bandit => self.bandit += s,
            Tier::BackgroundSweep => self.background_sweep += s,
        }
    }

    /// Sum over tiers.
    pub fn total(&self) -> f64 {
        self.interactive
            + self.pinned_lease
            + self.scheduled_plan
            + self.bandit
            + self.background_sweep
    }
}

/// Observation totals of a frequency range over a span: the C12 weighting input and the report's
/// coverage numbers (ADR-0012 §1.4). A range counts as observed at an instant only when it lies
/// entirely inside an observed window's covered extent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationTotals {
    /// Range.
    pub freq: FreqRange,
    /// Span.
    pub span: TimeRange,
    /// Distinct visits (a hop or dwell covering the range).
    pub n_visits: u64,
    /// Visits at activity-independent tiers.
    pub n_visits_activity_independent: u64,
    /// Observed seconds per tier.
    pub observed_s: TierSeconds,
    /// Longest gap between visits inside the span (span edges count), s.
    pub max_gap_s: f64,
    /// Mean start-to-start revisit interval, s; `None` with fewer than two visits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_revisit_s: Option<f64>,
}

impl ObservationTotals {
    /// Checks counts and non-negative, finite times.
    pub fn validate(&self) -> Result<(), ValidationError> {
        ensure(
            self.n_visits_activity_independent <= self.n_visits,
            "n_visits_activity_independent",
            "must not exceed n_visits",
        )?;
        let span_s = self.span.duration_ns() as f64 * 1e-9;
        ensure_in(self.max_gap_s, 0.0, span_s, "max_gap_s")?;
        ensure_in(self.observed_s.total(), 0.0, f64::MAX, "observed_s")?;
        if let Some(m) = self.mean_revisit_s {
            ensure_in(m, 0.0, span_s, "mean_revisit_s")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::SiteId;
    use crate::time::Timestamp;

    fn t(s: f64) -> Timestamp {
        Timestamp::from_unix_nanos((s * 1e9) as i64)
    }

    fn window() -> ObservedWindow {
        ObservedWindow {
            center_hz: 433.92e6,
            sample_rate_hz: 2e6,
            usable: FreqRange::new(433.12e6, 434.72e6),
            dc_excluded: Some(FreqRange::new(433.91e6, 433.93e6)),
            rbw_hz: 1e3,
        }
    }

    fn dwell() -> DwellRecord {
        DwellRecord {
            schema: 1,
            survey_id: None,
            seq: 7,
            plan_version: 1,
            site: SiteKey::Site(SiteId::new()),
            reason: Reason::Novelty { arm: 3, score: 0.8 },
            tier: Tier::Bandit,
            window: window(),
            rf_path: 0,
            planned: TimeRange::new(t(100.0), t(110.0)),
            observed: TimeRange::new(t(100.05), t(110.0)),
            preempted: false,
            dropped_samples: 0,
            overload: false,
            provenance_ref: None,
        }
    }

    #[test]
    fn tiers_order_by_priority() {
        assert!(Tier::Interactive > Tier::PinnedLease);
        assert!(Tier::PinnedLease > Tier::ScheduledPlan);
        assert!(Tier::ScheduledPlan > Tier::Bandit);
        assert!(Tier::Bandit > Tier::BackgroundSweep);
        assert!(Tier::HIGHEST_FIRST.windows(2).all(|w| w[0] > w[1]));
        assert!(Tier::BackgroundSweep.activity_independent());
        assert!(!Tier::Bandit.activity_independent());
    }

    #[test]
    fn reason_codes_serialise_and_read_as_text() {
        let r = Reason::Novelty { arm: 3, score: 0.8 };
        let json = serde_json::to_value(r).unwrap();
        assert_eq!(json["code"], "novelty");
        assert_eq!(r.text(), "novelty 0.80");
        assert_eq!(
            Reason::Lease {
                kind: LeaseKind::UserPin,
                lease: 1
            }
            .text(),
            "user pin"
        );
        assert_eq!(
            Reason::BackgroundSweep { hop: 0 }.text(),
            "background sweep"
        );
        assert_eq!(
            Reason::BeaconDue { arm: 0, eta_s: 1.0 }.text(),
            "beacon due"
        );
        let bad = serde_json::json!({"code": "novelty", "arm": 1, "score": 0.5, "extra": 1});
        assert!(serde_json::from_value::<Reason>(bad).is_err());
    }

    #[test]
    fn dwell_record_round_trips_and_validates() {
        let d = dwell();
        d.validate().unwrap();
        let rec = ObservationRecord::Dwell(d.clone());
        let json = serde_json::to_string(&rec).unwrap();
        assert_eq!(
            serde_json::from_str::<ObservationRecord>(&json).unwrap(),
            rec
        );
        assert!((d.observed_s() - 9.95).abs() < 1e-9);

        let mut v = serde_json::to_value(&d).unwrap();
        v["unknown"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<DwellRecord>(v).is_err(),
            "unknown fields rejected"
        );

        let mut wrong_tier = d.clone();
        wrong_tier.tier = Tier::Interactive;
        assert_eq!(wrong_tier.validate().unwrap_err().field, "tier");

        let mut outside = d.clone();
        outside.observed = TimeRange::new(t(99.0), t(110.0));
        assert_eq!(outside.validate().unwrap_err().field, "observed");

        let mut dc = d;
        dc.window.dc_excluded = Some(FreqRange::new(400e6, 401e6));
        assert!(dc.validate().is_err());
    }

    #[test]
    fn covered_extent_removes_the_dc_notch() {
        let c = window().covered();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0], FreqRange::new(433.12e6, 433.91e6));
        assert_eq!(c[1], FreqRange::new(433.93e6, 434.72e6));
    }

    #[test]
    fn sweep_record_bounds_and_order() {
        let mut s = SweepRecord {
            schema: 1,
            survey_id: None,
            plan_version: 1,
            site: SiteKey::Unassigned,
            geometry: 42,
            span: TimeRange::new(t(0.0), t(1.0)),
            visits: vec![
                HopVisit {
                    hop: 0,
                    start_ms: 0,
                    observed_ms: 45,
                },
                HopVisit {
                    hop: 1,
                    start_ms: 50,
                    observed_ms: 45,
                },
            ],
            preempted_hops: 0,
            dropped_samples: 0,
            overload_hops: 0,
        };
        s.validate().unwrap();
        s.visits.swap(0, 1);
        assert_eq!(s.validate().unwrap_err().field, "visits");
        s.visits.swap(0, 1);
        s.span = TimeRange::new(t(0.0), t(61.0));
        assert_eq!(s.validate().unwrap_err().field, "span");
    }
}
