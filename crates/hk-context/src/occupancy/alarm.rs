//! Novelty alarms (T-122, ADR-0012 §7): hysteresis per `AlarmKey`, suppression, provenance-first
//! explanation, anomaly rows + `AlarmDetail`, and hand-off to `crate::correlate`.
//!
//! # Flow
//!
//! 1. **Input.** A [`NoveltySnapshot`] per scored interval: the sample time, the site, the
//!    device's provenance steps near it ([`DeviceStep`]), and one [`AlarmInput`] per subject and
//!    kind. [`inputs_from_fold`] builds inputs from a T-119 fold (`level_z` → level above
//!    baseline, `occupancy_z` > 0 → busier, < 0 → **quieter than usual**, a latched change point
//!    → change point); [`new_emitter_input`] builds one from `new_emitter` novelty.
//! 2. **Merge.** Hot adjacent `Cells` inputs of one kind (novelty ≥ `off`, gap ≤
//!    `merge_gap_cells` baseline cells) merge into one subject; a group that really overlaps a
//!    tracked key of that kind reuses the best-overlapping key, so an open alarm is extended,
//!    never duplicated (§7.2). A dismissed key takes only a group inside its own extent, so a new
//!    neighbour or a wideband emitter over it is new activity. Idle keys are evicted after the
//!    cooldown.
//! 3. **Suppress** in the §7.3 order: mobile, unassigned, provenance-explained, immature,
//!    dismissed. Every suppression is counted per kind ([`SuppressionCounts`]).
//!    - **Explain the device first, never explain an emitter away (§7.4).** Calibration,
//!      spur-mask, antenna, overload, restart, drop and site steps in
//!      `[t − lookback, t + observed_s]` over the subject explain the change. A gain step explains
//!      it only with a known Δ, when at least `gain_breadth` (70 %) of at least
//!      `gain_breadth_min_subjects` observed subjects under it moved with Δ in the same snapshot
//!      (levels within ± `gain_tolerance_db` of Δ; occupancy hot in Δ's direction), and when the
//!      subject's own level cells shift by Δ ± tolerance (no residual). An explained change is
//!      self-inflicted: once per key and step, when its raw novelty reaches `on`, an
//!      [`AlarmAction::Explained`] anomaly is written with a top `self-inflicted` Explanation, and
//!      no novelty alarm is raised. A gain step that does not fit (unknown Δ, isolated group,
//!      residual) still lets the alarm raise and is written as a low-score `possible contributor`.
//! 4. **Hysteresis** ([`HysteresisState::step`] on the component novelty): raise → a new
//!    anomaly; reopen (re-raise within the 1 h cooldown, sample clock) → the same anomaly; hold →
//!    extend; clear. A subject not observed in an interval is not stepped (unobserved is not
//!    quiet). A dismissed key is not stepped until its dismissal expires (7 days, sample clock);
//!    after that it must re-raise from scratch.
//! 5. **Write** ([`AlarmWriter::apply`]): anomaly + [`AlarmRow`] + status in one transaction,
//!    then the explanation stages in [`ExplanationStage::ORDER`]: provenance (none survived),
//!    propagation and external events (the C30 [`Correlator`] over the cached feeds), own history
//!    (no rule yet), and `unexplained` with score `1 − best external score` — so an unknown
//!    emitter ranks `unexplained` first unless a cached event fits better.
//!
//! The engine is pure (no I/O, no wall clock); [`AlarmEngine::resume`] rebuilds it from
//! [`Repository::latest_alarm_rows`].

use std::collections::BTreeMap;

use hk_model::attention::ATTENTION_SCHEMA_VERSION;
use hk_model::attention::alarm::{
    AlarmDetail, AlarmKey, AlarmKind, AlarmSubject, AlarmTransition, AlarmUnit, ExplanationStage,
    HysteresisConfig, HysteresisState, Suppression, suppression,
};
use hk_model::attention::baseline::{BaselineResolution, CalKey, HourOfWeek, Maturity, SiteKey};
use hk_model::attention::report::ProvenanceStepKind;
use hk_model::attention::score::novelty_from_z;
use hk_model::repo::alarms::{AlarmLifecycle, AlarmRow, AlarmState};
use hk_model::{
    Anomaly, AnomalyId, AnomalyStatus, AnomalyStatusChange, AnomalySubject, Cause, CorrelationType,
    Evidence, Explanation, ExplanationId, FreqRange, Region, RepoError, Repository, TimeRange,
    Timestamp,
};
use hk_store::baseline::{BaselineSubject, ChangeStatistic};

use super::baseline::{FoldOutcome, IntervalObservation};
use super::novelty::NoveltyConfig;
use crate::correlate::{CorrelateError, Correlator};
use crate::feeds::FeedState;
use crate::geo::Site;

/// `Anomaly::detector_version` of novelty alarms.
pub const DETECTOR_VERSION: &str = "hk-context.c12-alarm@1";
/// `Explanation::rule_version` of the provenance and unexplained stages.
pub const RULE_VERSION: &str = "hk-context.c12-alarm@1";

/// Alarm settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AlarmConfig {
    /// Hysteresis (on 0.7, off 0.4, 2 up, 3 down, 1 h cooldown).
    pub hysteresis: HysteresisConfig,
    /// z → novelty mapping (3 → 0, 10 → 1).
    pub novelty: NoveltyConfig,
    /// How far before an interval a provenance step still explains it, s (one interval).
    pub provenance_lookback_s: f64,
    /// A dismissal lasts this long on the sample clock, s (7 days).
    pub dismissal_s: f64,
    /// Adjacent hot cells merge across gaps of up to this many baseline cells (2).
    pub merge_gap_cells: i64,
    /// Level-0 cells per baseline cell (16).
    pub cell_factor: i64,
    /// A gain step explains a level shift within ± this of Δ, dB (3, fixed, never scaled by Δ).
    pub gain_tolerance_db: f64,
    /// A gain step explains a subject only when at least this fraction of the snapshot's observed
    /// subjects under the step moved with it (0.7): a broadband shift, not an isolated group.
    pub gain_breadth: f64,
    /// ... and at least this many subjects were observed under the step (4). A snapshot with fewer
    /// cannot show a broadband shift, so a gain step never explains it away.
    pub gain_breadth_min_subjects: usize,
}

impl Default for AlarmConfig {
    fn default() -> Self {
        Self {
            hysteresis: HysteresisConfig::default(),
            novelty: NoveltyConfig::default(),
            provenance_lookback_s: 900.0,
            dismissal_s: 7.0 * 86_400.0,
            merge_gap_cells: 2,
            cell_factor: 16,
            gain_tolerance_db: 3.0,
            gain_breadth: 0.7,
            gain_breadth_min_subjects: 4,
        }
    }
}

/// A device provenance step (gain, calibration, spur mask, antenna, overload, restart, drop,
/// site) from the observation log or tile provenance.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceStep {
    /// When (sample clock).
    pub t: Timestamp,
    /// Kind.
    pub kind: ProvenanceStepKind,
    /// Affected extent; `None` = the whole front end.
    pub freq: Option<FreqRange>,
    /// Net gain change, dB, when known (gain steps).
    pub gain_delta_db: Option<f64>,
    /// Short description, e.g. `lna 16→32 dB`.
    pub detail: String,
}

impl DeviceStep {
    /// From a report provenance step. A gain step's delta is summed from its detail
    /// (`lna 16→32 dB, vga 20→24 dB` → +20 dB); any part without a dB size (amp, gain table, an
    /// unknown state) leaves it unknown, and an unknown delta never explains a change away.
    pub fn from_report(s: &hk_model::attention::report::ProvenanceStep) -> Self {
        Self {
            t: s.t,
            kind: s.kind,
            freq: s.freq,
            gain_delta_db: (s.kind == ProvenanceStepKind::Gain)
                .then(|| gain_delta_from_detail(&s.detail))
                .flatten(),
            detail: s.detail.clone(),
        }
    }
}

/// Net dB change of a report gain detail, `None` unless every part is `name a→b dB`.
fn gain_delta_from_detail(detail: &str) -> Option<f64> {
    let mut sum = 0.0;
    for part in detail.split(',').map(str::trim) {
        let body = part.strip_suffix(" dB")?;
        let (_, range) = body.rsplit_once(' ')?;
        let (a, b) = range.split_once('→')?;
        let (a, b): (f64, f64) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
        sum += b - a;
    }
    sum.is_finite().then_some(sum)
}

/// One subject's scored interval for one alarm kind.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AlarmInput {
    /// Kind.
    pub kind: AlarmKind,
    /// Subject.
    pub subject: AlarmSubject,
    /// Frequency extent of the subject.
    pub freq: FreqRange,
    /// Baseline calibration.
    pub cal: CalKey,
    /// Pool compared against.
    pub resolution: BaselineResolution,
    /// Slot compared against.
    pub slot: HourOfWeek,
    /// Maturity of the pool.
    pub maturity: Maturity,
    /// T-119 already knows a provenance step explains it (e.g. a new gain state).
    pub provenance_explained: bool,
    /// Unit.
    pub unit: AlarmUnit,
    /// Observed value.
    pub observed: f64,
    /// Baseline mean.
    pub baseline_mean: f64,
    /// Baseline spread.
    pub baseline_spread: f64,
    /// z-score (signed).
    pub z: f64,
    /// This kind's novelty, 0–1 (not forced to 0 for provenance: the engine decides).
    pub novelty: f64,
    /// Observed seconds behind the value.
    pub observed_s: f64,
}

/// One scored interval's alarm inputs (the T-128 hand-off).
#[derive(Clone, Debug, PartialEq)]
pub struct NoveltySnapshot {
    /// Interval start (sample clock).
    pub t: Timestamp,
    /// Site assignment at measurement time.
    pub site: SiteKey,
    /// Provenance steps near `t` (any order).
    pub steps: Vec<DeviceStep>,
    /// Inputs.
    pub inputs: Vec<AlarmInput>,
}

/// Suppressions counted per kind (§7.3).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SuppressionCounts {
    /// `(kind, suppression)` → count.
    pub counts: BTreeMap<(AlarmKind, SuppressionName), u64>,
}

/// [`Suppression`] as an ordered name (for maps and JSON).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SuppressionName(pub &'static str);

fn suppression_name(s: Suppression) -> SuppressionName {
    SuppressionName(match s {
        Suppression::ImmatureBaseline => "immature-baseline",
        Suppression::MobileSite => "mobile-site",
        Suppression::UnassignedSite => "unassigned-site",
        Suppression::ProvenanceExplained => "provenance-explained",
        Suppression::Dismissed => "dismissed",
    })
}

impl SuppressionCounts {
    fn bump(&mut self, kind: AlarmKind, s: Suppression) {
        *self.counts.entry((kind, suppression_name(s))).or_default() += 1;
    }

    /// Count for one kind and suppression.
    pub fn get(&self, kind: AlarmKind, s: Suppression) -> u64 {
        self.counts
            .get(&(kind, suppression_name(s)))
            .copied()
            .unwrap_or(0)
    }

    /// Total of one suppression over all kinds.
    pub fn total(&self, s: Suppression) -> u64 {
        let n = suppression_name(s);
        self.counts
            .iter()
            .filter(|((_, k), _)| *k == n)
            .map(|(_, v)| v)
            .sum()
    }
}

/// What the engine decided for one key in one interval.
#[derive(Clone, Debug, PartialEq)]
pub enum AlarmAction {
    /// Open a new alarm anomaly `anomaly`.
    Raise {
        /// Key.
        key: AlarmKey,
        /// Id assigned to the new anomaly.
        anomaly: AnomalyId,
        /// Evidence.
        input: AlarmInput,
        /// Intervals at or above `on`.
        intervals_above: u32,
        /// Device steps that coincide but do not account for the change (an unknown-size gain
        /// step, a gain step without a broad matching shift): annotated, never suppressing.
        contributors: Vec<DeviceStep>,
    },
    /// Re-open the key's last anomaly (cleared within the cooldown).
    Reopen {
        /// Key.
        key: AlarmKey,
        /// The anomaly re-opened.
        anomaly: AnomalyId,
        /// Evidence.
        input: AlarmInput,
    },
    /// Still open: extend.
    Hold {
        /// Key.
        key: AlarmKey,
        /// The open anomaly.
        anomaly: AnomalyId,
        /// Evidence.
        input: AlarmInput,
    },
    /// Cleared.
    Clear {
        /// Key.
        key: AlarmKey,
        /// The anomaly cleared.
        anomaly: AnomalyId,
    },
    /// A device step explains the change: write a self-inflicted anomaly, not a novelty alarm.
    Explained {
        /// Key.
        key: AlarmKey,
        /// Id assigned to the anomaly.
        anomaly: AnomalyId,
        /// Evidence.
        input: AlarmInput,
        /// The step.
        step: DeviceStep,
    },
}

#[derive(Clone, Copy, Debug, Default)]
struct KeyState {
    hyst: HysteresisState,
    anomaly: Option<AnomalyId>,
    above: u32,
    dismissed_until: Option<Timestamp>,
    explained_step: Option<Timestamp>,
    /// The last input for this key was provenance-explained (a persistent gain split writes one
    /// self-inflicted anomaly, not one per interval).
    explained_active: bool,
    /// Last snapshot that stepped this key (eviction of idle explained keys).
    last_seen: Option<Timestamp>,
}

/// Per-key alarm state machine (pure).
#[derive(Clone, Debug, Default)]
pub struct AlarmEngine {
    cfg: AlarmConfig,
    keys: BTreeMap<AlarmKey, KeyState>,
    counts: SuppressionCounts,
    now: Option<Timestamp>,
}

fn secs_between(a: Timestamp, b: Timestamp) -> f64 {
    (b.as_unix_nanos() - a.as_unix_nanos()) as f64 / 1e9
}

fn add_s(t: Timestamp, s: f64) -> Timestamp {
    t.saturating_add_nanos((s * 1e9) as i64)
}

/// Raw novelty of an input ignoring the zero rules: the component novelty or its z mapping.
fn raw_novelty(input: &AlarmInput, cfg: &NoveltyConfig) -> f64 {
    let from_z = match input.unit {
        AlarmUnit::Count => 0.0,
        _ => novelty_from_z(input.z.abs(), cfg.z_min, cfg.z_sat),
    };
    input.novelty.max(from_z).clamp(0.0, 1.0)
}

/// How a provenance step relates to one input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StepFit {
    /// The step accounts for the change: self-inflicted, not novel.
    Explains,
    /// The step coincides but does not account for the change: the alarm still raises, and the
    /// step is annotated as a possible contributor.
    Contributes,
    /// Outside the step's time window or extent.
    Unrelated,
}

/// Whether `step` applies to `input` at `t` (lookback window and extent).
fn step_applies(input: &AlarmInput, step: &DeviceStep, t: Timestamp, cfg: &AlarmConfig) -> bool {
    let lo = add_s(t, -cfg.provenance_lookback_s);
    let hi = add_s(t, input.observed_s.max(0.0));
    step.t >= lo && step.t <= hi && step.freq.is_none_or(|f| f.overlaps(&input.freq))
}

fn is_level(i: &AlarmInput) -> bool {
    i.kind == AlarmKind::LevelAboveBaseline && i.unit == AlarmUnit::Db
}

fn is_occupancy(i: &AlarmInput) -> bool {
    matches!(
        i.kind,
        AlarmKind::BusierThanUsual | AlarmKind::QuieterThanUsual
    )
}

/// A dB input whose shift from its baseline is Δ within the fixed tolerance.
fn level_matches(i: &AlarmInput, delta: f64, cfg: &AlarmConfig) -> bool {
    let shift = i.observed - i.baseline_mean;
    shift.signum() == delta.signum() && (shift - delta).abs() <= cfg.gain_tolerance_db
}

/// An occupancy input that moved (hot) in Δ's direction.
fn occupancy_moved(i: &AlarmInput, delta: f64, cfg: &AlarmConfig) -> bool {
    i.z.signum() == delta.signum() && raw_novelty(i, &cfg.novelty) >= cfg.hysteresis.off
}

type Member = fn(&AlarmInput) -> bool;
type Moved = fn(&AlarmInput, f64, &AlarmConfig) -> bool;

/// §7.4 "a broadband shift ≈ the gain delta across cells under that gain state": at least
/// `gain_breadth` of the snapshot's observed `member` subjects under the step moved with Δ (quiet
/// subjects count in the denominator), and at least `gain_breadth_min_subjects` were observed.
fn broad(
    snap: &NoveltySnapshot,
    step: &DeviceStep,
    delta: f64,
    cfg: &AlarmConfig,
    member: Member,
    moved: Moved,
) -> bool {
    let (mut n, mut m) = (0usize, 0usize);
    for i in snap
        .inputs
        .iter()
        .filter(|i| member(i) && step_applies(i, step, snap.t, cfg))
    {
        n += 1;
        m += usize::from(moved(i, delta, cfg));
    }
    n >= cfg.gain_breadth_min_subjects.max(1) && m as f64 >= cfg.gain_breadth * n as f64
}

/// Explain the device first, without explaining a real emitter away: a non-gain step in the
/// window explains the change; a gain step explains it only with a known Δ, a broad matching shift
/// across the snapshot, and no level residual beyond Δ ± tolerance on the subject's own cells.
fn step_fit(
    input: &AlarmInput,
    step: &DeviceStep,
    snap: &NoveltySnapshot,
    cfg: &AlarmConfig,
) -> StepFit {
    if !step_applies(input, step, snap.t, cfg) {
        return StepFit::Unrelated;
    }
    if step.kind != ProvenanceStepKind::Gain {
        return StepFit::Explains;
    }
    let Some(delta) = step.gain_delta_db.filter(|d| d.is_finite() && *d != 0.0) else {
        // Unknown size: never explains activity away, only a possible contributor.
        return StepFit::Contributes;
    };
    // Gain compensation: every level cell under the step overlapping this subject must shift by
    // ≈ Δ. A residual far beyond Δ, or a cell that stays idle once Δ is removed, is not the gain.
    let levels: Vec<&AlarmInput> = snap
        .inputs
        .iter()
        .filter(|i| is_level(i) && i.freq.overlaps(&input.freq))
        .filter(|i| step_applies(i, step, snap.t, cfg))
        .collect();
    let compensated = levels.iter().all(|i| level_matches(i, delta, cfg));
    let fits = compensated
        && match input.kind {
            AlarmKind::LevelAboveBaseline | AlarmKind::ChangePoint
                if input.unit == AlarmUnit::Db =>
            {
                level_matches(input, delta, cfg)
                    && broad(snap, step, delta, cfg, is_level, level_matches)
            }
            // A new emitter needs its own cells to show the gain shift, and a broadband one.
            AlarmKind::NewEmitter => {
                !levels.is_empty() && broad(snap, step, delta, cfg, is_level, level_matches)
            }
            kind => {
                let direction = match kind {
                    AlarmKind::BusierThanUsual => delta > 0.0,
                    AlarmKind::QuieterThanUsual => delta < 0.0,
                    // A fraction change point: the direction is in the z sign.
                    _ => input.z.signum() == delta.signum(),
                };
                direction && broad(snap, step, delta, cfg, is_occupancy, occupancy_moved)
            }
        };
    if fits {
        StepFit::Explains
    } else {
        StepFit::Contributes
    }
}

fn cells(s: &AlarmSubject) -> Option<(u16, i64, i64)> {
    match *s {
        AlarmSubject::Cells {
            scheme,
            lo_cell,
            hi_cell,
        } => Some((scheme, lo_cell, hi_cell)),
        _ => None,
    }
}

impl AlarmEngine {
    /// An engine with `cfg` (hysteresis validated).
    pub fn new(cfg: AlarmConfig) -> Result<Self, String> {
        cfg.hysteresis.validate().map_err(|e| e.to_string())?;
        let non_negative = |v: f64| v.is_finite() && v >= 0.0;
        if !non_negative(cfg.dismissal_s)
            || !non_negative(cfg.provenance_lookback_s)
            || cfg.cell_factor < 1
            || cfg.merge_gap_cells < 0
        {
            return Err(format!("alarm config {cfg:?}"));
        }
        Ok(Self {
            cfg,
            ..Self::default()
        })
    }

    /// Rebuilds state from the newest row per key.
    pub fn resume(cfg: AlarmConfig, repo: &Repository) -> Result<Self, String> {
        let mut e = Self::new(cfg)?;
        for row in repo.latest_alarm_rows().map_err(|e| e.to_string())? {
            let open = row.state == AlarmState::Open;
            let st = e.keys.entry(row.key).or_default();
            st.hyst = HysteresisState::restored(open, row.cleared_at);
            st.anomaly = (row.state != AlarmState::Explained).then_some(row.anomaly_id);
            st.dismissed_until = row
                .dismissed_until
                .filter(|_| row.state == AlarmState::Dismissed);
            if row.state == AlarmState::Explained {
                // The step time, so the same step never writes a second self-inflicted row.
                st.explained_step = Some(row.explained_step_t.unwrap_or(row.raised_at));
                st.explained_active = true;
            }
            st.last_seen = Some(row.last_t);
            e.now = e.now.max(Some(row.last_t));
        }
        Ok(e)
    }

    /// Settings.
    pub fn config(&self) -> &AlarmConfig {
        &self.cfg
    }

    /// Suppression counts so far.
    pub fn suppressions(&self) -> &SuppressionCounts {
        &self.counts
    }

    /// Counts evidence the baseline could not score (an immature pool, a mobile or unassigned
    /// site): one suppression per kind (§7.3 "counted…, never silently dropped"), never a raise
    /// and no key state. `kinds` come from [`unscored_evidence`]; a mature site with nothing to
    /// suppress counts nothing.
    pub fn count_unscored(
        &mut self,
        t: Timestamp,
        site: SiteKey,
        maturity: Maturity,
        provenance_explained: bool,
        kinds: &[AlarmKind],
    ) {
        self.now = self.now.max(Some(t));
        if let Some(s) = suppression(site, maturity, provenance_explained) {
            for kind in kinds {
                self.counts.bump(*kind, s);
            }
        }
    }

    /// Latest sample time seen.
    pub fn now(&self) -> Option<Timestamp> {
        self.now
    }

    /// Open alarm keys and their anomalies.
    pub fn open_alarms(&self) -> Vec<(AlarmKey, AnomalyId)> {
        self.keys
            .iter()
            .filter(|(_, s)| s.hyst.is_open())
            .filter_map(|(k, s)| s.anomaly.map(|a| (*k, a)))
            .collect()
    }

    /// Records a user dismissal of `key` at `t` (sample clock): until it expires the key is
    /// suppressed and not stepped; afterwards it must re-raise. Returns the expiry.
    pub fn dismiss(&mut self, key: AlarmKey, t: Timestamp) -> Timestamp {
        let until = add_s(t, self.cfg.dismissal_s);
        let st = self.keys.entry(key).or_default();
        st.dismissed_until = Some(until);
        st.hyst = HysteresisState::default();
        st.above = 0;
        until
    }

    /// Lifts a dismissal and marks `anomaly` open again (user reopen).
    pub fn reopen(&mut self, key: AlarmKey, anomaly: AnomalyId) {
        let st = self.keys.entry(key).or_default();
        st.dismissed_until = None;
        st.hyst = HysteresisState::restored(true, st.hyst.last_cleared());
        st.anomaly = Some(anomaly);
    }

    /// Merges hot adjacent cells and maps groups onto tracked keys.
    fn keyed(
        &self,
        site: hk_model::ids::SiteId,
        inputs: &[AlarmInput],
        t: Timestamp,
    ) -> Vec<(AlarmKey, AlarmInput)> {
        let gap = self.cfg.merge_gap_cells * self.cfg.cell_factor;
        let hot = |i: &AlarmInput| raw_novelty(i, &self.cfg.novelty) >= self.cfg.hysteresis.off;
        let mut out: Vec<(AlarmKey, AlarmInput)> = Vec::new();
        let mut by_kind: BTreeMap<(AlarmKind, u16), Vec<AlarmInput>> = BTreeMap::new();
        for i in inputs {
            match cells(&i.subject) {
                Some((scheme, _, _)) => by_kind.entry((i.kind, scheme)).or_default().push(*i),
                None => out.push((
                    AlarmKey {
                        kind: i.kind,
                        site,
                        subject: i.subject,
                    },
                    *i,
                )),
            }
        }
        for ((kind, scheme), mut list) in by_kind {
            list.sort_by_key(|i| cells(&i.subject).map(|c| c.1));
            // Groups of hot cells (hull + strongest evidence).
            let mut groups: Vec<(i64, i64, AlarmInput)> = Vec::new();
            let mut cold: Vec<(i64, i64, AlarmInput)> = Vec::new();
            for i in list {
                let (_, lo, hi) = cells(&i.subject).expect("cells");
                if !hot(&i) {
                    cold.push((lo, hi, i));
                    continue;
                }
                match groups.last_mut() {
                    Some(g) if lo - g.1 <= gap => {
                        g.1 = g.1.max(hi);
                        g.2.freq = FreqRange::new(
                            g.2.freq.lo_hz.min(i.freq.lo_hz),
                            g.2.freq.hi_hz.max(i.freq.hi_hz),
                        );
                        if raw_novelty(&i, &self.cfg.novelty) > raw_novelty(&g.2, &self.cfg.novelty)
                        {
                            let freq = g.2.freq;
                            g.2 = i;
                            g.2.freq = freq;
                        }
                    }
                    _ => groups.push((lo, hi, i)),
                }
            }
            let tracked: Vec<(AlarmKey, i64, i64)> = self
                .keys
                .keys()
                .filter(|k| k.kind == kind && k.site == site)
                .filter_map(|k| {
                    cells(&k.subject)
                        .filter(|c| c.0 == scheme)
                        .map(|c| (*k, c.1, c.2))
                })
                .collect();
            let dismissed = |k: &AlarmKey| {
                self.keys
                    .get(k)
                    .and_then(|s| s.dismissed_until)
                    .is_some_and(|u| t < u)
            };
            let mut used: Vec<AlarmKey> = Vec::new();
            for (lo, hi, mut input) in groups {
                // A key is reused only on real overlap, the best (overlap / union) winning. A
                // dismissed key takes only a group inside its extent: a new neighbour or a wideband
                // emitter over it is new activity, never swallowed by the dismissal.
                let key = tracked
                    .iter()
                    .filter(|(k, _, _)| !used.contains(k))
                    .filter_map(|(k, a, b)| {
                        let overlap = hi.min(*b) - lo.max(*a);
                        if overlap <= 0 || (dismissed(k) && (lo < *a || hi > *b)) {
                            return None;
                        }
                        let union = (hi.max(*b) - lo.min(*a)).max(1);
                        Some((overlap as f64 / union as f64, *k))
                    })
                    .fold(None::<(f64, AlarmKey)>, |best, c| match best {
                        Some(b) if b.0 >= c.0 => Some(b),
                        _ => Some(c),
                    })
                    .map(|(_, k)| k)
                    .unwrap_or(AlarmKey {
                        kind,
                        site,
                        subject: AlarmSubject::Cells {
                            scheme,
                            lo_cell: lo,
                            hi_cell: hi,
                        },
                    });
                input.subject = key.subject;
                used.push(key);
                out.push((key, input));
            }
            // Tracked keys observed only by cold cells step with their strongest cold novelty.
            for (key, a, b) in tracked.iter().filter(|(k, _, _)| !used.contains(k)) {
                let best = cold
                    .iter()
                    .filter(|(lo, hi, _)| *lo < *b && *hi > *a)
                    .max_by(|x, y| {
                        raw_novelty(&x.2, &self.cfg.novelty)
                            .total_cmp(&raw_novelty(&y.2, &self.cfg.novelty))
                    });
                if let Some((_, _, i)) = best {
                    let mut i = *i;
                    i.subject = key.subject;
                    out.push((*key, i));
                }
            }
        }
        out
    }

    /// Steps every key the snapshot observed and returns the actions to write.
    pub fn step(&mut self, snap: &NoveltySnapshot) -> Vec<AlarmAction> {
        let t = snap.t;
        self.now = self.now.max(Some(t));
        let site = match snap.site {
            SiteKey::Site(id) => id,
            other => {
                for i in &snap.inputs {
                    if let Some(s) = suppression(other, i.maturity, false) {
                        self.counts.bump(i.kind, s);
                    }
                }
                return Vec::new();
            }
        };
        let mut actions = Vec::new();
        let hcfg = self.cfg.hysteresis;
        for (key, input) in self.keyed(site, &snap.inputs, t) {
            let raw = raw_novelty(&input, &self.cfg.novelty);
            let real_step = !input.provenance_explained;
            let mut contributors: Vec<DeviceStep> = Vec::new();
            let explaining = if input.provenance_explained {
                // T-119 already compared against a new gain state (its per-state split).
                Some(DeviceStep {
                    t,
                    kind: ProvenanceStepKind::Gain,
                    freq: None,
                    gain_delta_db: None,
                    detail: "front-end state changed (new baseline gain state)".into(),
                })
            } else {
                let mut best: Option<&DeviceStep> = None;
                for s in &snap.steps {
                    match step_fit(&input, s, snap, &self.cfg) {
                        StepFit::Explains => {
                            if best.is_none_or(|b| s.t >= b.t) {
                                best = Some(s);
                            }
                        }
                        StepFit::Contributes => contributors.push(s.clone()),
                        StepFit::Unrelated => {}
                    }
                }
                best.cloned()
            };
            if let Some(sup) = suppression(snap.site, input.maturity, explaining.is_some()) {
                self.counts.bump(input.kind, sup);
                if let (Suppression::ProvenanceExplained, Some(step)) = (sup, explaining) {
                    let st = self.keys.entry(key).or_default();
                    st.last_seen = Some(t);
                    let fresh =
                        !st.explained_active || (real_step && st.explained_step != Some(step.t));
                    if raw >= hcfg.on && fresh {
                        st.explained_active = true;
                        st.explained_step = Some(step.t);
                        actions.push(AlarmAction::Explained {
                            key,
                            anomaly: AnomalyId::new(),
                            input,
                            step,
                        });
                    }
                }
                continue;
            }
            if raw < hcfg.off && !self.keys.contains_key(&key) {
                continue; // nothing tracked, nothing to track
            }
            let st = self.keys.entry(key).or_default();
            st.explained_active = false;
            st.last_seen = Some(t);
            if let Some(until) = st.dismissed_until {
                if t < until {
                    self.counts.bump(input.kind, Suppression::Dismissed);
                    continue;
                }
                st.dismissed_until = None;
            }
            let novelty = input.novelty.clamp(0.0, 1.0);
            st.above = if novelty >= hcfg.on { st.above + 1 } else { 0 };
            match st.hyst.step(novelty, t, &hcfg) {
                AlarmTransition::Quiet => {}
                AlarmTransition::Raise => {
                    let anomaly = AnomalyId::new();
                    st.anomaly = Some(anomaly);
                    actions.push(AlarmAction::Raise {
                        key,
                        anomaly,
                        input,
                        intervals_above: st.above.max(hcfg.on_intervals),
                        contributors,
                    });
                    st.above = 0;
                }
                AlarmTransition::Reopen => match st.anomaly {
                    Some(anomaly) => actions.push(AlarmAction::Reopen {
                        key,
                        anomaly,
                        input,
                    }),
                    None => {
                        let anomaly = AnomalyId::new();
                        st.anomaly = Some(anomaly);
                        actions.push(AlarmAction::Raise {
                            key,
                            anomaly,
                            input,
                            intervals_above: hcfg.on_intervals,
                            contributors,
                        });
                    }
                },
                AlarmTransition::Hold => {
                    if let Some(anomaly) = st.anomaly {
                        actions.push(AlarmAction::Hold {
                            key,
                            anomaly,
                            input,
                        });
                    }
                }
                AlarmTransition::Clear => {
                    if let Some(anomaly) = st.anomaly {
                        actions.push(AlarmAction::Clear { key, anomaly });
                    }
                }
            }
        }
        self.evict(t);
        actions
    }

    /// Drops keys with nothing left to remember, bounding memory: not open, not building toward a
    /// raise, no live dismissal, cleared longer ago than the re-raise cooldown, and (explained
    /// keys) not seen within the cooldown. A later raise on an evicted key is a new alarm, as it
    /// would be after the cooldown anyway.
    fn evict(&mut self, now: Timestamp) {
        let cooldown = self.cfg.hysteresis.cooldown_s;
        let recent = |t: Option<Timestamp>| t.is_some_and(|t| secs_between(t, now) < cooldown);
        self.keys.retain(|_, st| {
            st.hyst.is_open()
                || st.above > 0
                || st.dismissed_until.is_some_and(|u| now < u)
                || recent(st.hyst.last_cleared())
                || (st.explained_active && recent(st.last_seen))
        });
    }
}

// ---- Inputs from T-119 ----

/// Frequency extent of a baseline subject (level-0 cell width `cell_hz`, `cell_factor` per
/// baseline cell) and its alarm subject (cells as level-0 indices).
pub fn subject_of(
    subject: &BaselineSubject,
    scheme: u16,
    cell_hz: f64,
    cell_factor: i64,
) -> (AlarmSubject, FreqRange) {
    match subject {
        BaselineSubject::Cell { index } => {
            let (lo, hi) = (index * cell_factor, (index + 1) * cell_factor);
            (
                AlarmSubject::Cells {
                    scheme,
                    lo_cell: lo,
                    hi_cell: hi,
                },
                FreqRange::new(lo as f64 * cell_hz, hi as f64 * cell_hz),
            )
        }
        BaselineSubject::Channel { key } => (
            AlarmSubject::Channel { key: *key },
            FreqRange::new(key.lo_cell as f64 * cell_hz, key.hi_cell as f64 * cell_hz),
        ),
    }
}

/// Pool context of a fold (from `baseline::pools`), when the caller has it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PoolContext {
    /// Pool compared against (`AllHours` when unknown).
    pub resolution: Option<BaselineResolution>,
    /// Level pool mean and σ, dB.
    pub level: Option<(f64, f64)>,
    /// Occupancy pool FCO and σ of the interval FCO.
    pub occupancy: Option<(f64, f64)>,
}

/// Alarm inputs of one T-119 fold: `level_z` → level above baseline, signed `occupancy_z` →
/// busier/quieter than usual, a change point → change point. Without pool context the baseline
/// mean/spread are back-computed from z (σ = 1 dB for levels; binomial σ for FCO).
#[allow(clippy::too_many_arguments)]
pub fn inputs_from_fold(
    obs: &IntervalObservation,
    fold: &FoldOutcome,
    cal: CalKey,
    slot: HourOfWeek,
    scheme: u16,
    cell_hz: f64,
    cell_factor: i64,
    pool: PoolContext,
    cfg: &NoveltyConfig,
) -> Vec<AlarmInput> {
    let n = &fold.novelty;
    let (subject, freq) = subject_of(&obs.subject, scheme, cell_hz, cell_factor);
    let resolution = pool
        .resolution
        .or(match n.maturity {
            Maturity::Mature { resolution } => Some(resolution),
            Maturity::Immature { .. } => None,
        })
        .unwrap_or(BaselineResolution::AllHours);
    let base = |kind, unit, observed: f64, mean: f64, spread: f64, z: f64| AlarmInput {
        kind,
        subject,
        freq,
        cal,
        resolution,
        slot,
        maturity: n.maturity,
        provenance_explained: n.provenance_explained,
        unit,
        observed,
        baseline_mean: mean,
        baseline_spread: spread.max(0.0),
        z,
        novelty: novelty_from_z(z.abs(), cfg.z_min, cfg.z_sat),
        observed_s: obs.observed_s,
    };
    let mut out = Vec::new();
    if let (Some(z), Some(level)) = (n.level_z, obs.level_db) {
        let (mean, sigma) = pool.level.unwrap_or((level - z, 1.0));
        out.push(base(
            AlarmKind::LevelAboveBaseline,
            AlarmUnit::Db,
            level,
            mean,
            sigma,
            z,
        ));
    }
    if let (Some(z), Some(fco)) = (n.occupancy_z, obs.fco()) {
        let (mean, sigma) = pool.occupancy.unwrap_or_else(|| {
            let s = (fco * (1.0 - fco) / obs.n_eff.max(1.0)).sqrt().max(1e-6);
            ((fco - z * s).clamp(0.0, 1.0), s)
        });
        let kind = if z >= 0.0 {
            AlarmKind::BusierThanUsual
        } else {
            AlarmKind::QuieterThanUsual
        };
        out.push(base(kind, AlarmUnit::Fraction, fco, mean, sigma, z));
    }
    if let Some(cp) = fold.change_point {
        let (unit, observed, mean, spread) = match cp.statistic {
            ChangeStatistic::Level => (
                AlarmUnit::Db,
                obs.level_db.unwrap_or(0.0),
                pool.level.map_or(obs.level_db.unwrap_or(0.0), |p| p.0),
                pool.level.map_or(1.0, |p| p.1),
            ),
            _ => (
                AlarmUnit::Fraction,
                obs.fco().unwrap_or(0.0),
                pool.occupancy.map_or(obs.fco().unwrap_or(0.0), |p| p.0),
                pool.occupancy.map_or(0.0, |p| p.1),
            ),
        };
        let z = f64::from(cp.direction) * cp.cusum.abs();
        let mut i = base(AlarmKind::ChangePoint, unit, observed, mean, spread, z);
        // A latched change point is a decided event: full novelty while it stays latched.
        i.novelty = 1.0;
        out.push(i);
    }
    out
}

/// Kinds with evidence in an **immature** fold that [`inputs_from_fold`] could not score (no z
/// against an immature pool): a level observed → `level-above-baseline`, an occupancy observed →
/// `busier-than-usual` (the occupancy kind; its sign is unknown without a pool). Empty for a
/// mature fold. Feed to [`AlarmEngine::count_unscored`]; never compute a z from them.
pub fn unscored_evidence(obs: &IntervalObservation, fold: &FoldOutcome) -> Vec<AlarmKind> {
    let n = &fold.novelty;
    if n.maturity.is_mature() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if obs.level_db.is_some() && n.level_z.is_none() {
        out.push(AlarmKind::LevelAboveBaseline);
    }
    if obs.fco().is_some() && n.occupancy_z.is_none() {
        out.push(AlarmKind::BusierThanUsual);
    }
    out
}

/// A new-emitter input: `k` first sightings where `expected` were due at the site's rate, with
/// novelty `new_emitter` (`FirstSightingRate::novelty`).
#[allow(clippy::too_many_arguments)]
pub fn new_emitter_input(
    emitter: hk_model::ids::EmitterId,
    freq: FreqRange,
    cal: CalKey,
    slot: HourOfWeek,
    maturity: Maturity,
    k: u64,
    expected: f64,
    new_emitter: f64,
    observed_s: f64,
) -> AlarmInput {
    AlarmInput {
        kind: AlarmKind::NewEmitter,
        subject: AlarmSubject::Emitter { id: emitter },
        freq,
        cal,
        resolution: match maturity {
            Maturity::Mature { resolution } => resolution,
            Maturity::Immature { .. } => BaselineResolution::AllHours,
        },
        slot,
        maturity,
        provenance_explained: false,
        unit: AlarmUnit::Count,
        observed: k as f64,
        baseline_mean: expected.max(0.0),
        baseline_spread: expected.max(0.0).sqrt(),
        z: if expected > 0.0 {
            (k as f64 - expected) / expected.sqrt()
        } else {
            k as f64
        },
        novelty: new_emitter.clamp(0.0, 1.0),
        observed_s,
    }
}

// ---- Writing ----

/// One written transition (for the `anomalies` stream and callers).
#[derive(Clone, Debug, PartialEq)]
pub struct AlarmEvent {
    /// Transition.
    pub transition: AlarmLifecycle,
    /// The anomaly.
    pub anomaly: Anomaly,
    /// Its detail row after the transition.
    pub row: AlarmRow,
    /// Latest explanations, best first.
    pub explanations: Vec<Explanation>,
}

/// Writes engine actions to the repository and runs the explanation stages.
#[derive(Clone, Debug, Default)]
pub struct AlarmWriter {
    /// The C30 correlator (stages 2–3).
    pub correlator: Correlator,
}

fn detail_of(
    key: AlarmKey,
    input: &AlarmInput,
    intervals_above: u32,
    stages: &[ExplanationStage],
) -> AlarmDetail {
    AlarmDetail {
        schema: ATTENTION_SCHEMA_VERSION,
        key,
        cal: input.cal,
        resolution: input.resolution,
        slot: input.slot,
        unit: input.unit,
        observed: input.observed,
        baseline_mean: input.baseline_mean,
        baseline_spread: input.baseline_spread,
        z: input.z,
        novelty: input.novelty.clamp(0.0, 1.0),
        intervals_above,
        observed_s: input.observed_s.max(0.0),
        stages_applied: stages.to_vec(),
    }
}

/// Latest (not superseded) explanations of an anomaly, best first.
pub fn latest_explanations(
    repo: &Repository,
    id: AnomalyId,
) -> Result<Vec<Explanation>, RepoError> {
    let all = repo.explanations_for_anomaly(id)?;
    let superseded: std::collections::BTreeSet<ExplanationId> =
        all.iter().filter_map(|e| e.supersedes).collect();
    let mut latest: Vec<Explanation> = all
        .into_iter()
        .filter(|e| !superseded.contains(&e.id))
        .collect();
    latest.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.t.cmp(&b.t)));
    Ok(latest)
}

/// Alarm write errors.
#[derive(Debug, thiserror::Error)]
pub enum AlarmError {
    /// Repository.
    #[error(transparent)]
    Repo(#[from] RepoError),
    /// Correlation.
    #[error(transparent)]
    Correlate(#[from] CorrelateError),
}

impl AlarmWriter {
    /// Applies `actions` at `t`: rows, statuses and explanations. `feed_states` and `site` go to
    /// the correlator. Actions are written in order; the first failure stops the batch.
    pub fn apply(
        &self,
        repo: &mut Repository,
        actions: &[AlarmAction],
        t: Timestamp,
        feed_states: &BTreeMap<String, FeedState>,
        site: Option<&Site>,
    ) -> Result<Vec<AlarmEvent>, AlarmError> {
        let mut out = Vec::new();
        for action in actions {
            out.push(match action {
                AlarmAction::Raise {
                    key,
                    anomaly,
                    input,
                    intervals_above,
                    contributors,
                } => self.raise(
                    repo,
                    *key,
                    *anomaly,
                    input,
                    (*intervals_above, contributors),
                    t,
                    feed_states,
                    site,
                )?,
                AlarmAction::Explained {
                    key,
                    anomaly,
                    input,
                    step,
                } => self.explained(repo, *key, *anomaly, input, step, t)?,
                AlarmAction::Reopen { anomaly, input, .. } => {
                    let (a, mut row) = load(repo, *anomaly)?;
                    row.state = AlarmState::Open;
                    row.last_transition = AlarmLifecycle::Reopened;
                    row.reopen_count += 1;
                    row.last_t = t.max(row.last_t);
                    row.freq = hull(row.freq, input.freq);
                    row.detail = detail_of(
                        row.key,
                        input,
                        row.detail.intervals_above,
                        &row.detail.stages_applied,
                    );
                    let status = status(*anomaly, AnomalyStatus::Open, t, "reopened");
                    repo.update_alarm(&row, Some(&status))?;
                    self.correlator
                        .correlate_with_states(repo, feed_states, *anomaly, site, t)?;
                    self.event(repo, AlarmLifecycle::Reopened, a, row)?
                }
                AlarmAction::Hold { anomaly, input, .. } => {
                    let (a, mut row) = load(repo, *anomaly)?;
                    row.last_transition = AlarmLifecycle::Held;
                    row.last_t = t.max(row.last_t);
                    row.freq = hull(row.freq, input.freq);
                    row.detail = detail_of(
                        row.key,
                        input,
                        row.detail.intervals_above,
                        &row.detail.stages_applied,
                    );
                    repo.update_alarm(&row, None)?;
                    self.event(repo, AlarmLifecycle::Held, a, row)?
                }
                AlarmAction::Clear { anomaly, .. } => {
                    let (a, mut row) = load(repo, *anomaly)?;
                    row.state = AlarmState::Cleared;
                    row.last_transition = AlarmLifecycle::Cleared;
                    row.cleared_at = Some(t);
                    row.last_t = t.max(row.last_t);
                    let status = status(*anomaly, AnomalyStatus::Resolved, t, "cleared");
                    repo.update_alarm(&row, Some(&status))?;
                    self.event(repo, AlarmLifecycle::Cleared, a, row)?
                }
            });
        }
        Ok(out)
    }

    fn event(
        &self,
        repo: &Repository,
        transition: AlarmLifecycle,
        anomaly: Anomaly,
        row: AlarmRow,
    ) -> Result<AlarmEvent, AlarmError> {
        let explanations = latest_explanations(repo, anomaly.id)?;
        Ok(AlarmEvent {
            transition,
            anomaly,
            row,
            explanations,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn raise(
        &self,
        repo: &mut Repository,
        key: AlarmKey,
        id: AnomalyId,
        input: &AlarmInput,
        (intervals_above, contributors): (u32, &Vec<DeviceStep>),
        t: Timestamp,
        feed_states: &BTreeMap<String, FeedState>,
        site: Option<&Site>,
    ) -> Result<AlarmEvent, AlarmError> {
        let anomaly = anomaly_of(key, id, input, t);
        let row = AlarmRow {
            anomaly_id: id,
            key,
            state: AlarmState::Open,
            last_transition: AlarmLifecycle::Raised,
            raised_at: t,
            last_t: t,
            reopen_count: 0,
            cleared_at: None,
            dismissed_until: None,
            freq: input.freq,
            detail: detail_of(key, input, intervals_above, &ExplanationStage::ORDER),
            explained_step_t: None,
        };
        repo.insert_alarm(
            &anomaly,
            &row,
            &[status(id, AnomalyStatus::Open, t, "raised")],
        )?;
        // Stage 1 (provenance) found nothing, or the engine would not have raised. Stages 2–3:
        // propagation and external events from the cached feeds.
        let outcome = self
            .correlator
            .correlate_with_states(repo, feed_states, id, site, t)?;
        let best = outcome
            .candidates
            .iter()
            .map(|c| c.score)
            .fold(0.0_f64, f64::max);
        // Stage 4 (own history) has no rule yet. Stage 5: unexplained, ranked below any fit.
        let value = |name: &str, value: f64| Evidence::Value {
            name: name.to_owned(),
            value,
        };
        repo.insert_explanation(&Explanation {
            id: ExplanationId::new(),
            anomaly_ref: id,
            cause: Cause::Unexplained,
            correlation_type: CorrelationType::Signature,
            score: (1.0 - best).clamp(0.0, 1.0),
            evidence: vec![
                Evidence::History {
                    region: anomaly.region,
                },
                value("novelty", input.novelty),
                value("z", input.z),
                value("best_external_score", best),
            ],
            supersedes: None,
            provisional: false,
            rule_version: RULE_VERSION.into(),
            t,
        })?;
        // Device steps that coincided without accounting for the change: a possible contributor,
        // ranked well below `unexplained`, never a resolution.
        for step in contributors {
            let kind = serde_json::to_value(step.kind)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            let mut evidence = vec![value("step_lag_s", secs_between(step.t, t))];
            if let Some(d) = step.gain_delta_db {
                evidence.push(value("gain_delta_db", d));
            }
            evidence.push(value(
                "observed_shift",
                input.observed - input.baseline_mean,
            ));
            repo.insert_explanation(&Explanation {
                id: ExplanationId::new(),
                anomaly_ref: id,
                cause: Cause::SelfInflicted {
                    reason: format!(
                        "possible contributor, {kind}: {} (does not account for the change)",
                        step.detail
                    ),
                },
                correlation_type: CorrelationType::TimeCoincidence,
                score: (0.5 * (1.0 - best)).clamp(0.0, 0.2),
                evidence,
                supersedes: None,
                provisional: true,
                rule_version: RULE_VERSION.into(),
                t,
            })?;
        }
        self.event(repo, AlarmLifecycle::Raised, anomaly, row)
    }

    fn explained(
        &self,
        repo: &mut Repository,
        key: AlarmKey,
        id: AnomalyId,
        input: &AlarmInput,
        step: &DeviceStep,
        t: Timestamp,
    ) -> Result<AlarmEvent, AlarmError> {
        let mut anomaly = anomaly_of(key, id, input, t);
        anomaly.score = raw_novelty(input, &NoveltyConfig::default());
        let mut detail = detail_of(key, input, 0, &[ExplanationStage::Provenance]);
        detail.novelty = 0.0; // forced to 0 (§4.4)
        let row = AlarmRow {
            anomaly_id: id,
            key,
            state: AlarmState::Explained,
            last_transition: AlarmLifecycle::Explained,
            raised_at: t,
            last_t: t,
            reopen_count: 0,
            cleared_at: None,
            dismissed_until: None,
            freq: input.freq,
            detail,
            explained_step_t: Some(step.t),
        };
        repo.insert_alarm(
            &anomaly,
            &row,
            &[
                status(id, AnomalyStatus::Open, t, "explained"),
                status(id, AnomalyStatus::Resolved, t, "self-inflicted"),
            ],
        )?;
        let kind = serde_json::to_value(step.kind)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let value = |name: &str, value: f64| Evidence::Value {
            name: name.to_owned(),
            value,
        };
        let mut evidence = vec![
            Evidence::History {
                region: anomaly.region,
            },
            value("step_lag_s", secs_between(step.t, t)),
            value("observed_shift", input.observed - input.baseline_mean),
        ];
        if let Some(d) = step.gain_delta_db {
            evidence.push(value("gain_delta_db", d));
        }
        repo.insert_explanation(&Explanation {
            id: ExplanationId::new(),
            anomaly_ref: id,
            cause: Cause::SelfInflicted {
                reason: format!("{kind}: {}", step.detail),
            },
            correlation_type: CorrelationType::TimeCoincidence,
            score: 1.0,
            evidence,
            supersedes: None,
            provisional: false,
            rule_version: RULE_VERSION.into(),
            t,
        })?;
        self.event(repo, AlarmLifecycle::Explained, anomaly, row)
    }
}

fn hull(a: FreqRange, b: FreqRange) -> FreqRange {
    FreqRange::new(a.lo_hz.min(b.lo_hz), a.hi_hz.max(b.hi_hz))
}

fn status(id: AnomalyId, status: AnomalyStatus, t: Timestamp, note: &str) -> AnomalyStatusChange {
    AnomalyStatusChange {
        anomaly_id: id,
        status,
        t,
        note: Some(note.to_owned()),
    }
}

fn load(repo: &Repository, id: AnomalyId) -> Result<(Anomaly, AlarmRow), AlarmError> {
    let a = repo.anomaly(id)?;
    let row = repo.alarm_row(id)?.ok_or_else(|| RepoError::NotFound {
        kind: "alarm",
        id: id.to_string(),
    })?;
    Ok((a, row))
}

fn anomaly_of(key: AlarmKey, id: AnomalyId, input: &AlarmInput, t: Timestamp) -> Anomaly {
    let end = add_s(t, input.observed_s.max(0.0));
    Anomaly {
        id,
        kind: key.kind.anomaly_kind(),
        subject: match key.subject {
            AlarmSubject::Emitter { id } => AnomalySubject::Emitter(id),
            _ => AnomalySubject::Region,
        },
        region: Region::new(input.freq, TimeRange::new(t, end)),
        score: input.novelty.clamp(0.0, 1.0),
        baseline_ref: Some(key.baseline_ref(input.cal, input.resolution, input.slot)),
        t,
        detector_version: DETECTOR_VERSION.into(),
    }
}

#[cfg(test)]
mod tests;
