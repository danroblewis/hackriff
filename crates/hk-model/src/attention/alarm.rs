//! Novelty alarms (ADR-0012 §7): kinds, dedupe keys, hysteresis and suppression.
//!
//! An alarm is persisted as an [`Anomaly`](crate::Anomaly) row (so the existing C30 correlation
//! and Explanation machinery applies) whose `baseline_ref` is [`AlarmKey::baseline_ref`], plus an
//! [`AlarmDetail`] (T-122 stores it beside the anomaly).

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::baseline::{BaselineResolution, CalKey, HourOfWeek, Maturity, SiteKey};
use super::occupancy::ChannelKey;
use super::{ValidationError, ensure, ensure_in, ensure_schema};
use crate::context::AnomalyKind;
use crate::ids::{CalibrationStateId, EmitterId, SiteId};
use crate::time::Timestamp;

/// Alarm kinds (§7.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AlarmKind {
    /// Level above the baseline pool (dB z-score).
    LevelAboveBaseline,
    /// A new emitter entered the inventory where the site's new-emitter rate makes it surprising.
    NewEmitter,
    /// FCO above the baseline pool ("busier than usual").
    BusierThanUsual,
    /// FCO below the baseline pool ("quieter than usual": a usual transmitter went silent).
    /// Additive (T-122): occupancy novelty is two-sided, so the sign picks the kind.
    QuieterThanUsual,
    /// The adaptive baseline diverged from the frozen reference (CUSUM).
    ChangePoint,
}

impl AlarmKind {
    /// The [`AnomalyKind`] the alarm is stored as.
    pub fn anomaly_kind(self) -> AnomalyKind {
        match self {
            AlarmKind::LevelAboveBaseline => AnomalyKind::LevelAboveBaseline,
            AlarmKind::NewEmitter => AnomalyKind::NewEmitter,
            AlarmKind::BusierThanUsual => AnomalyKind::BusierThanBaseline,
            AlarmKind::QuieterThanUsual => AnomalyKind::QuieterThanBaseline,
            AlarmKind::ChangePoint => AnomalyKind::ChangePoint,
        }
    }

    /// Stable kebab-case name.
    pub fn as_str(self) -> &'static str {
        match self {
            AlarmKind::LevelAboveBaseline => "level-above-baseline",
            AlarmKind::NewEmitter => "new-emitter",
            AlarmKind::BusierThanUsual => "busier-than-usual",
            AlarmKind::QuieterThanUsual => "quieter-than-usual",
            AlarmKind::ChangePoint => "change-point",
        }
    }

    /// Every kind.
    pub const ALL: [AlarmKind; 5] = [
        AlarmKind::LevelAboveBaseline,
        AlarmKind::NewEmitter,
        AlarmKind::BusierThanUsual,
        AlarmKind::QuieterThanUsual,
        AlarmKind::ChangePoint,
    ];

    /// Parses [`AlarmKind::as_str`].
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

fn resolution_str(r: BaselineResolution) -> &'static str {
    match r {
        BaselineResolution::HourOfWeek => "hour-of-week",
        BaselineResolution::HourOfDay => "hour-of-day",
        BaselineResolution::DayPart => "day-part",
        BaselineResolution::AllHours => "all-hours",
    }
}

fn parse_resolution(s: &str) -> Option<BaselineResolution> {
    BaselineResolution::FINEST_FIRST
        .into_iter()
        .find(|r| resolution_str(*r) == s)
}

/// What an alarm is about; part of the dedupe key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum AlarmSubject {
    /// A learned channel.
    Channel {
        /// Key.
        key: ChannelKey,
    },
    /// Merged adjacent baseline cells, as level-0 cell indices.
    Cells {
        /// Pyramid scheme.
        scheme: u16,
        /// First level-0 cell.
        lo_cell: i64,
        /// One past the last.
        hi_cell: i64,
    },
    /// An inventory emitter.
    Emitter {
        /// Id.
        id: EmitterId,
    },
}

/// Dedupe key (§7.2): while an alarm with this key is open it is extended, never duplicated; a
/// re-raise within the cooldown re-opens it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlarmKey {
    /// Kind.
    pub kind: AlarmKind,
    /// Site (alarms exist only for discrete sites).
    pub site: SiteId,
    /// Subject.
    pub subject: AlarmSubject,
}

/// A parsed `baseline_ref`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlarmRef {
    /// Key.
    pub key: AlarmKey,
    /// Calibration of the baseline.
    pub cal: CalKey,
    /// Pool compared against.
    pub resolution: BaselineResolution,
    /// Slot compared against.
    pub slot: HourOfWeek,
}

impl AlarmKey {
    /// Prefix and version of the `baseline_ref` format.
    pub const PREFIX: &'static str = "c12-alarm:v1";

    /// The `Anomaly::baseline_ref` string:
    /// `c12-alarm:v1;kind=…;site=<uuid>;cal=none|<uuid>;res=…;slot=<0–167>;subject=…` where subject
    /// is `channel:<scheme>:<lo>..<hi>`, `cells:<scheme>:<lo>..<hi>` or `emitter:<uuid>`.
    pub fn baseline_ref(
        &self,
        cal: CalKey,
        resolution: BaselineResolution,
        slot: HourOfWeek,
    ) -> String {
        let cal = match cal {
            CalKey::Uncalibrated => "none".to_string(),
            CalKey::Calibrated(id) => id.to_string(),
        };
        let subject = match self.subject {
            AlarmSubject::Channel { key } => {
                format!("channel:{}:{}..{}", key.scheme, key.lo_cell, key.hi_cell)
            }
            AlarmSubject::Cells {
                scheme,
                lo_cell,
                hi_cell,
            } => format!("cells:{scheme}:{lo_cell}..{hi_cell}"),
            AlarmSubject::Emitter { id } => format!("emitter:{id}"),
        };
        format!(
            "{};kind={};site={};cal={};res={};slot={};subject={}",
            Self::PREFIX,
            self.kind.as_str(),
            self.site,
            cal,
            resolution_str(resolution),
            slot.index(),
            subject
        )
    }

    /// Parses [`AlarmKey::baseline_ref`] output; `None` for anything else (e.g. floor episodes).
    pub fn parse_baseline_ref(s: &str) -> Option<AlarmRef> {
        let mut parts = s.split(';');
        if parts.next()? != Self::PREFIX {
            return None;
        }
        let (mut kind, mut site, mut cal, mut res, mut slot, mut subject) =
            (None, None, None, None, None, None);
        for p in parts {
            let (k, v) = p.split_once('=')?;
            match k {
                "kind" => kind = AlarmKind::parse(v),
                "site" => site = SiteId::from_str(v).ok(),
                "cal" => {
                    cal = if v == "none" {
                        Some(CalKey::Uncalibrated)
                    } else {
                        CalibrationStateId::from_str(v).ok().map(CalKey::Calibrated)
                    }
                }
                "res" => res = parse_resolution(v),
                "slot" => {
                    slot = v
                        .parse::<u8>()
                        .ok()
                        .and_then(|n| HourOfWeek::try_from(n).ok())
                }
                "subject" => subject = parse_subject(v),
                _ => return None,
            }
        }
        Some(AlarmRef {
            key: AlarmKey {
                kind: kind?,
                site: site?,
                subject: subject?,
            },
            cal: cal?,
            resolution: res?,
            slot: slot?,
        })
    }
}

fn parse_subject(v: &str) -> Option<AlarmSubject> {
    let (tag, rest) = v.split_once(':')?;
    let cells = |rest: &str| -> Option<(u16, i64, i64)> {
        let (scheme, range) = rest.split_once(':')?;
        let (lo, hi) = range.split_once("..")?;
        let (lo, hi) = (lo.parse().ok()?, hi.parse().ok()?);
        (hi > lo).then_some((scheme.parse().ok()?, lo, hi))
    };
    match tag {
        "channel" => cells(rest).map(|(scheme, lo_cell, hi_cell)| AlarmSubject::Channel {
            key: ChannelKey {
                scheme,
                lo_cell,
                hi_cell,
            },
        }),
        "cells" => cells(rest).map(|(scheme, lo_cell, hi_cell)| AlarmSubject::Cells {
            scheme,
            lo_cell,
            hi_cell,
        }),
        "emitter" => EmitterId::from_str(rest)
            .ok()
            .map(|id| AlarmSubject::Emitter { id }),
        _ => None,
    }
}

/// Hysteresis and dedupe settings (§7.2).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HysteresisConfig {
    /// Novelty at or above which a scored interval counts toward raising (default 0.7).
    pub on: f64,
    /// Novelty below which an interval counts toward clearing (default 0.4).
    pub off: f64,
    /// Consecutive intervals at or above `on` to raise (default 2).
    pub on_intervals: u32,
    /// Consecutive intervals below `off` to clear (default 3).
    pub off_intervals: u32,
    /// After clearing, a re-raise within this many seconds re-opens the same alarm (default 3600).
    pub cooldown_s: f64,
}

impl Default for HysteresisConfig {
    fn default() -> Self {
        Self {
            on: 0.7,
            off: 0.4,
            on_intervals: 2,
            off_intervals: 3,
            cooldown_s: 3600.0,
        }
    }
}

impl HysteresisConfig {
    /// `0 < off < on ≤ 1`, interval counts 1–100, cooldown ≥ 0.
    pub fn validate(&self) -> Result<(), ValidationError> {
        ensure_in(self.on, f64::MIN_POSITIVE, 1.0, "hysteresis.on")?;
        ensure_in(self.off, f64::MIN_POSITIVE, self.on, "hysteresis.off")?;
        ensure(self.off < self.on, "hysteresis.off", "must be below on")?;
        ensure(
            (1..=100).contains(&self.on_intervals),
            "hysteresis.on_intervals",
            "must be 1–100",
        )?;
        ensure(
            (1..=100).contains(&self.off_intervals),
            "hysteresis.off_intervals",
            "must be 1–100",
        )?;
        ensure_in(
            self.cooldown_s,
            0.0,
            30.0 * 86_400.0,
            "hysteresis.cooldown_s",
        )
    }
}

/// What one scored interval does to an alarm key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlarmTransition {
    /// Nothing open, nothing raised.
    Quiet,
    /// Open a new alarm.
    Raise,
    /// Re-open the alarm cleared within the cooldown (append an `open` status; no new row).
    Reopen,
    /// Stays open (extend its region/time).
    Hold,
    /// Resolve it.
    Clear,
}

/// Per-key hysteresis state (§7.2).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HysteresisState {
    above: u32,
    below: u32,
    open: bool,
    last_cleared: Option<Timestamp>,
}

impl HysteresisState {
    /// State rebuilt from the repository on resume (T-122): open or not, and the last clear.
    pub fn restored(open: bool, last_cleared: Option<Timestamp>) -> Self {
        Self {
            above: 0,
            below: 0,
            open,
            last_cleared,
        }
    }

    /// When the key last cleared.
    pub fn last_cleared(&self) -> Option<Timestamp> {
        self.last_cleared
    }

    /// Whether an alarm is open for this key.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Feeds one scored interval's novelty at `now`.
    pub fn step(
        &mut self,
        novelty: f64,
        now: Timestamp,
        cfg: &HysteresisConfig,
    ) -> AlarmTransition {
        if self.open {
            if novelty < cfg.off {
                self.below += 1;
                if self.below >= cfg.off_intervals {
                    self.open = false;
                    self.below = 0;
                    self.last_cleared = Some(now);
                    return AlarmTransition::Clear;
                }
            } else {
                self.below = 0;
            }
            return AlarmTransition::Hold;
        }
        if novelty < cfg.on {
            self.above = 0;
            return AlarmTransition::Quiet;
        }
        self.above += 1;
        if self.above < cfg.on_intervals {
            return AlarmTransition::Quiet;
        }
        self.above = 0;
        self.open = true;
        let cooldown_ns = (cfg.cooldown_s * 1e9) as i64;
        match self.last_cleared {
            Some(c) if now.as_unix_nanos() - c.as_unix_nanos() <= cooldown_ns => {
                AlarmTransition::Reopen
            }
            _ => AlarmTransition::Raise,
        }
    }
}

/// Why a would-be alarm was not raised (§7.3). Counted and exposed, never silently dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Suppression {
    /// No mature pool.
    ImmatureBaseline,
    /// Device moving.
    MobileSite,
    /// No site.
    UnassignedSite,
    /// A provenance step explains it: written as an anomaly whose top explanation is
    /// `self-inflicted`, not as a novelty alarm.
    ProvenanceExplained,
    /// The user dismissed this key; suppressed until the dismissal expires.
    Dismissed,
}

/// The suppression that applies before any novelty alarm (checked in this order), if any.
pub fn suppression(
    site: SiteKey,
    maturity: Maturity,
    provenance_explained: bool,
) -> Option<Suppression> {
    match site {
        SiteKey::Mobile => Some(Suppression::MobileSite),
        SiteKey::Unassigned => Some(Suppression::UnassignedSite),
        SiteKey::Site(_) if provenance_explained => Some(Suppression::ProvenanceExplained),
        SiteKey::Site(_) if !maturity.is_mature() => Some(Suppression::ImmatureBaseline),
        SiteKey::Site(_) => None,
    }
}

/// Explanation stages, in the order C30 applies them to an alarm (§7.4): the device itself first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExplanationStage {
    /// Gain/cal/spur-mask/antenna/overload/restart steps (`Cause::SelfInflicted`).
    Provenance,
    /// Propagation and space weather (Es, tropo, flares, geomagnetic storms).
    Propagation,
    /// Other cached external events (GNSS jamming, passes, launches, lightning).
    ExternalEvent,
    /// Patterns in the device's own history (a known periodic emitter, a weekly pattern).
    OwnHistory,
    /// Nothing fits.
    Unexplained,
}

impl ExplanationStage {
    /// Stages in application order.
    pub const ORDER: [ExplanationStage; 5] = [
        ExplanationStage::Provenance,
        ExplanationStage::Propagation,
        ExplanationStage::ExternalEvent,
        ExplanationStage::OwnHistory,
        ExplanationStage::Unexplained,
    ];
}

/// Unit of an alarm's values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AlarmUnit {
    /// dB (level).
    Db,
    /// Fraction (FCO).
    Fraction,
    /// Count (new emitters).
    Count,
}

/// Evidence stored with an alarm (§7.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlarmDetail {
    /// [`super::ATTENTION_SCHEMA_VERSION`].
    pub schema: u32,
    /// Dedupe key.
    pub key: AlarmKey,
    /// Baseline calibration.
    pub cal: CalKey,
    /// Pool compared against.
    pub resolution: BaselineResolution,
    /// Slot compared against.
    pub slot: HourOfWeek,
    /// Unit of the values.
    pub unit: AlarmUnit,
    /// Observed value.
    pub observed: f64,
    /// Baseline mean.
    pub baseline_mean: f64,
    /// Baseline spread (σ for dB; binomial σ for fractions; expected count for counts).
    pub baseline_spread: f64,
    /// z-score.
    pub z: f64,
    /// Novelty, 0–1.
    pub novelty: f64,
    /// Consecutive intervals above `on` when raised.
    pub intervals_above: u32,
    /// Observed seconds behind the observed value.
    pub observed_s: f64,
    /// Explanation stages applied, in order, before raising.
    pub stages_applied: Vec<ExplanationStage>,
}

impl AlarmDetail {
    /// Checks schema, finiteness, novelty range and stage order (a prefix of
    /// [`ExplanationStage::ORDER`] starting at `provenance`).
    pub fn validate(&self) -> Result<(), ValidationError> {
        ensure_schema(self.schema)?;
        for (v, f) in [
            (self.observed, "observed"),
            (self.baseline_mean, "baseline_mean"),
            (self.z, "z"),
        ] {
            ensure(v.is_finite(), f, "must be finite")?;
        }
        ensure_in(self.baseline_spread, 0.0, f64::MAX, "baseline_spread")?;
        ensure_in(self.novelty, 0.0, 1.0, "novelty")?;
        ensure_in(self.observed_s, 0.0, f64::MAX, "observed_s")?;
        ensure(
            !self.stages_applied.is_empty()
                && self
                    .stages_applied
                    .iter()
                    .zip(ExplanationStage::ORDER)
                    .all(|(a, b)| *a == b),
            "stages_applied",
            "must start with provenance and follow the stage order",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: i64) -> Timestamp {
        Timestamp::from_unix_nanos(s * 1_000_000_000)
    }

    #[test]
    fn baseline_ref_round_trips_every_subject() {
        let site = SiteId::new();
        let slot = HourOfWeek::try_from(37).unwrap();
        for (subject, cal) in [
            (
                AlarmSubject::Channel {
                    key: ChannelKey {
                        scheme: 1,
                        lo_cell: 69_425,
                        hi_cell: 69_429,
                    },
                },
                CalKey::Uncalibrated,
            ),
            (
                AlarmSubject::Cells {
                    scheme: 1,
                    lo_cell: 0,
                    hi_cell: 16,
                },
                CalKey::Calibrated(CalibrationStateId::new()),
            ),
            (
                AlarmSubject::Emitter {
                    id: EmitterId::new(),
                },
                CalKey::Uncalibrated,
            ),
        ] {
            let key = AlarmKey {
                kind: AlarmKind::BusierThanUsual,
                site,
                subject,
            };
            let s = key.baseline_ref(cal, BaselineResolution::HourOfDay, slot);
            assert!(s.starts_with("c12-alarm:v1;kind=busier-than-usual;"));
            let r = AlarmKey::parse_baseline_ref(&s).unwrap();
            assert_eq!(
                r,
                AlarmRef {
                    key,
                    cal,
                    resolution: BaselineResolution::HourOfDay,
                    slot
                }
            );
        }
        assert!(AlarmKey::parse_baseline_ref("floor-episode:v1;run=a").is_none());
        assert!(AlarmKey::parse_baseline_ref("c12-alarm:v1;kind=new-emitter").is_none());
        assert_eq!(
            AlarmKind::BusierThanUsual.anomaly_kind(),
            AnomalyKind::BusierThanBaseline
        );
        assert_eq!(
            serde_json::to_value(AlarmKind::LevelAboveBaseline.anomaly_kind()).unwrap(),
            "level-above-baseline"
        );
    }

    #[test]
    fn hysteresis_raises_holds_clears_and_reopens() {
        let cfg = HysteresisConfig::default();
        cfg.validate().unwrap();
        let mut h = HysteresisState::default();
        assert_eq!(
            h.step(0.9, t(0), &cfg),
            AlarmTransition::Quiet,
            "one interval is not enough"
        );
        assert_eq!(
            h.step(0.1, t(1), &cfg),
            AlarmTransition::Quiet,
            "streak broken"
        );
        assert_eq!(h.step(0.9, t(2), &cfg), AlarmTransition::Quiet);
        assert_eq!(h.step(0.8, t(3), &cfg), AlarmTransition::Raise);
        assert!(h.is_open());
        assert_eq!(
            h.step(0.5, t(4), &cfg),
            AlarmTransition::Hold,
            "between off and on holds"
        );
        assert_eq!(h.step(0.1, t(5), &cfg), AlarmTransition::Hold);
        assert_eq!(h.step(0.1, t(6), &cfg), AlarmTransition::Hold);
        assert_eq!(h.step(0.1, t(7), &cfg), AlarmTransition::Clear);
        assert_eq!(h.step(0.9, t(8), &cfg), AlarmTransition::Quiet);
        assert_eq!(
            h.step(0.9, t(9), &cfg),
            AlarmTransition::Reopen,
            "within cooldown"
        );
        for s in 10..13 {
            h.step(0.0, t(s), &cfg);
        }
        h.step(0.9, t(10_000), &cfg);
        assert_eq!(
            h.step(0.9, t(10_001), &cfg),
            AlarmTransition::Raise,
            "after cooldown"
        );
        let bad = HysteresisConfig { off: 0.8, ..cfg };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn suppression_order() {
        let mature = Maturity::Mature {
            resolution: BaselineResolution::AllHours,
        };
        let immature = Maturity::Immature { observed_s: 1.0 };
        let site = SiteKey::Site(SiteId::new());
        assert_eq!(
            suppression(SiteKey::Mobile, mature, false),
            Some(Suppression::MobileSite)
        );
        assert_eq!(
            suppression(SiteKey::Unassigned, mature, true),
            Some(Suppression::UnassignedSite)
        );
        assert_eq!(
            suppression(site, immature, true),
            Some(Suppression::ProvenanceExplained)
        );
        assert_eq!(
            suppression(site, immature, false),
            Some(Suppression::ImmatureBaseline)
        );
        assert_eq!(suppression(site, mature, false), None);
    }

    #[test]
    fn detail_requires_provenance_first() {
        let mut d = AlarmDetail {
            schema: 1,
            key: AlarmKey {
                kind: AlarmKind::LevelAboveBaseline,
                site: SiteId::new(),
                subject: AlarmSubject::Cells {
                    scheme: 1,
                    lo_cell: 0,
                    hi_cell: 16,
                },
            },
            cal: CalKey::Uncalibrated,
            resolution: BaselineResolution::AllHours,
            slot: HourOfWeek::try_from(0).unwrap(),
            unit: AlarmUnit::Db,
            observed: -60.0,
            baseline_mean: -100.0,
            baseline_spread: 2.0,
            z: 20.0,
            novelty: 1.0,
            intervals_above: 2,
            observed_s: 30.0,
            stages_applied: vec![ExplanationStage::Provenance, ExplanationStage::Propagation],
        };
        d.validate().unwrap();
        d.stages_applied = vec![ExplanationStage::ExternalEvent];
        assert_eq!(d.validate().unwrap_err().field, "stages_applied");
    }
}
