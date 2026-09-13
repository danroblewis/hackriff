//! Anomaly lifecycle from noise-floor episodes (C08 → C30; AWARE-006).
//!
//! The lifecycle consumes [`EpisodeSignal`], an internal adapter over the floor tracker's events.
//! [`signal_from_floor_event`] is the **only** place that knows `hk_dsp::floor::FloorEvent`, so a
//! change to the tracker's event model changes that mapping, not the lifecycle.
//!
//! # Rules
//!
//! An episode is represented by **parts**: anomalies of the run, each belonging to one episode.
//! One part per episode is its **primary**, which follows the episode's extent; other parts come
//! from merged or split episodes and keep their own region (and their Explanations).
//!
//! - **Opened** (`Rise`) of an accepted class opens a primary `Anomaly(noise-floor-rise)` for its
//!   extent. Accepted: `NoiseLike` (and `Unverified` when configured). Structured episodes never
//!   open one; the tracker emits them, so the class is filtered here.
//! - **Opened with `split_from`** (the tracker split a disconnected episode): the parent's open
//!   parts lying mostly (≥ 50 % of their width) inside the new extent move to the new episode,
//!   clipped to it; the widest becomes its primary. If none moves, a primary is opened.
//! - **Extended** (the added extent): the episode's primary grows to cover it. Without an open
//!   primary, the added extent opens one. So a region widening on both sides stays one open
//!   anomaly. An Extend of a non-accepted class (structured blocks merged into a noise-like
//!   episode) is ignored, so the anomaly covers only the accepted blocks.
//! - **No overlap.** After an Extend or split, the episode's other open parts that overlap its
//!   primary are absorbed: the primary grows to their hull, their Explanations are copied to it
//!   and they are resolved `absorbed;by=<primary>` (in [`LifecycleReport::superseded`]). A split
//!   also clips the parent's parts off the new extent when it lies at one of their ends. So open
//!   anomalies never overlap.
//! - **Deferred (T-029):** every extent change inserts a successor row (supersession, below). An
//!   in-place region update would need a mutable anomaly region in the hk-model schema.
//! - **Updated** (the episode's current extent): open parts are clipped to it; parts with no
//!   overlap of positive width are resolved (`extent-returned`).
//! - **Closed** (`End`): `Merged` re-parents every open part to the surviving episode (they stay
//!   open with their Explanations); any other reason resolves every open part of the episode.
//! - **Unknown** (tracker lost the episode, e.g. after a gain change): open parts of that episode
//!   (or of the whole run) are resolved with `episode-state-unknown`; a later
//!   `Opened { continued }` re-opens them.
//! - **Fell** (a floor fall, possibly interrupted): open parts overlapping the fall are resolved
//!   (`floor-fall`), since a fall below the baseline means the rise is over.
//!
//! **Growing and shrinking.** Anomaly rows are append-only, so a part changes extent by
//! **supersession**: a successor anomaly with the new frequency extent (same kind, score,
//! `baseline_ref`, and the time span extended to now) is inserted, the old one is resolved with
//! `superseded;by=<id>`, and the old anomaly's latest Explanations are copied to the successor
//! (`supersedes` = the copied explanation; ones whose evidence is stale are skipped). Successors
//! appear in [`LifecycleReport::opened`] (correlate them) and in
//! [`LifecycleReport::superseded`].
//!
//! # Identity, duplicates, restarts
//!
//! A part's identity is `(run, episode, onset)` from the episode that first opened it; `run`
//! names one tracker instance (episode ids are unique per tracker only). The key is written into
//! `Anomaly::baseline_ref` as
//! `floor-episode:v1;run=…;segment=…;episode=…;onset_ns=…;baseline_dbfs_per_hz=…`, and the
//! current episode and primary flag of a re-parented, split or superseding part are written as
//! `…;episode=<id>;primary=<0|1>` status notes. The repository is the source of truth;
//! [`FloorAnomalies::resume`] rebuilds the in-memory index from both. A replayed Rise for a known
//! key, a replayed Extend already covered, or a replayed End of a closed episode writes nothing.
//! An anomaly left open by a tracker run that no longer exists is closed by [`close_orphaned`].
//!
//! The anomaly's region time is `[onset, confirmation]` (extended to the latest supersession);
//! the end time is the `resolved` entry. Not for the real-time path: each signal is a SQLite
//! transaction.

use std::collections::BTreeSet;

use hk_dsp::floor::{EndReason, FloorChangeClass, FloorEvent, FloorEventKind};
use hk_model::{
    Anomaly, AnomalyId, AnomalyKind, AnomalyStatus, AnomalyStatusChange, AnomalySubject,
    Explanation, ExplanationId, FreqRange, Region, RepoError, Repository, TimeRange, Timestamp,
};

/// Detector id written to `Anomaly::detector_version`.
pub const DETECTOR_VERSION: &str = "hk-context.floor-anomaly@1";
const KEY_PREFIX: &str = "floor-episode:v1";

/// Discriminator verdict of an episode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EpisodeClass {
    /// Steady and Gaussian: a floor change.
    NoiseLike,
    /// A wide structured emission, not a floor rise.
    Structured,
    /// Steady, no SK to check.
    Unverified,
}

/// An episode extent with its onset (a Rise, or the part an Extend added).
#[derive(Clone, Debug, PartialEq)]
pub struct EpisodeExtent {
    /// Episode id (unique per tracker run).
    pub episode: u64,
    /// Tracker segment.
    pub segment: u64,
    /// Verdict.
    pub class: EpisodeClass,
    /// First elevated frame.
    pub onset: Timestamp,
    /// Confirmation frame.
    pub confirmed: Timestamp,
    /// Lower edge, Hz.
    pub f_lo_hz: f64,
    /// Upper edge, Hz.
    pub f_hi_hz: f64,
    /// Step over the baseline, dB.
    pub step_db: f64,
    /// Statistical uncertainty of the step, dB.
    pub step_uncertainty_db: f64,
    /// Baseline (slow floor before), dBFS/Hz.
    pub baseline_dbfs_per_hz: f64,
}

/// Why an episode closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CloseReason {
    /// Back at the baseline.
    Returned,
    /// The tracker segment reset.
    Reset,
    /// Bridged into another episode, which continues (its parts are re-parented, not closed).
    Merged,
    /// Open for the tracker's `rebaseline_s`: the elevated level became the floor.
    Rebaselined,
}

/// The lifecycle's view of floor-tracker output.
#[derive(Clone, Debug, PartialEq)]
pub enum EpisodeSignal {
    /// A confirmed rise.
    Opened {
        /// Extent.
        extent: EpisodeExtent,
        /// Continuation after a reset: re-open the episode's closed parts.
        continued: bool,
        /// The episode split off this (still open) episode.
        split_from: Option<u64>,
    },
    /// The episode grew: the added extent with its own onset.
    Extended(EpisodeExtent),
    /// The episode's extent changed as blocks returned: its current extent.
    Updated {
        /// Episode.
        episode: u64,
        /// When.
        t: Timestamp,
        /// Current lower edge, Hz.
        f_lo_hz: f64,
        /// Current upper edge, Hz.
        f_hi_hz: f64,
    },
    /// The episode ended.
    Closed {
        /// Episode.
        episode: u64,
        /// When (first returned frame, or the resetting / merging frame).
        t: Timestamp,
        /// Why.
        reason: CloseReason,
        /// Episode length, s.
        duration_s: f64,
        /// For [`CloseReason::Merged`]: the episode that continues.
        merged_into: Option<u64>,
    },
    /// The tracker no longer knows the episode's state (`None`: any episode of the run).
    Unknown {
        /// Episode, if known.
        episode: Option<u64>,
        /// When.
        t: Timestamp,
    },
    /// A confirmed floor fall over an extent (possibly interrupted).
    Fell {
        /// When.
        t: Timestamp,
        /// Lower edge, Hz.
        f_lo_hz: f64,
        /// Upper edge, Hz.
        f_hi_hz: f64,
        /// Most of the fall's blocks were back near the old floor on some frames.
        interrupted: bool,
    },
}

/// Maps a floor-tracker event to a lifecycle signal. The single coupling point to hk-dsp.
pub fn signal_from_floor_event(e: &FloorEvent) -> Option<EpisodeSignal> {
    let class = match e.class {
        FloorChangeClass::NoiseLike => EpisodeClass::NoiseLike,
        FloorChangeClass::Structured => EpisodeClass::Structured,
        FloorChangeClass::Unverified => EpisodeClass::Unverified,
    };
    let extent = |f_lo_hz: f64, f_hi_hz: f64| EpisodeExtent {
        episode: e.episode,
        segment: e.segment,
        class,
        onset: e.onset_t.host_time,
        confirmed: e.confirmed_t.host_time,
        f_lo_hz,
        f_hi_hz,
        step_db: f64::from(e.step_db),
        step_uncertainty_db: f64::from(e.step_uncertainty_db),
        baseline_dbfs_per_hz: f64::from(e.baseline_dbfs_per_hz),
    };
    Some(match e.kind {
        FloorEventKind::Rise => EpisodeSignal::Opened {
            extent: extent(e.f_lo_hz, e.f_hi_hz),
            continued: false,
            split_from: e.split_from,
        },
        FloorEventKind::Extend => {
            EpisodeSignal::Extended(extent(e.change_f_lo_hz, e.change_f_hi_hz))
        }
        FloorEventKind::Update => EpisodeSignal::Updated {
            episode: e.episode,
            t: e.onset_t.host_time,
            f_lo_hz: e.f_lo_hz,
            f_hi_hz: e.f_hi_hz,
        },
        FloorEventKind::End => EpisodeSignal::Closed {
            episode: e.episode,
            t: e.onset_t.host_time,
            reason: match e.end_reason {
                Some(EndReason::Reset) => CloseReason::Reset,
                Some(EndReason::Merged) => CloseReason::Merged,
                Some(EndReason::Rebaselined) => CloseReason::Rebaselined,
                Some(EndReason::Returned) | None => CloseReason::Returned,
            },
            duration_s: e.duration_s,
            merged_into: e.merged_into,
        },
        FloorEventKind::Unknown => EpisodeSignal::Unknown {
            episode: Some(e.episode),
            t: e.onset_t.host_time,
        },
        FloorEventKind::Fall => EpisodeSignal::Fell {
            t: e.confirmed_t.host_time,
            f_lo_hz: e.f_lo_hz,
            f_hi_hz: e.f_hi_hz,
            interrupted: e.interrupted,
        },
    })
}

/// Lifecycle settings.
#[derive(Clone, Debug, PartialEq)]
pub struct FloorAnomalyConfig {
    /// Tracker run id: `[A-Za-z0-9._:-]+`, e.g. `<device>:<tracker start ns>`.
    pub run: String,
    /// Also open anomalies for `Unverified` episodes (no SK). Default off (AWARE-006 is NoiseLike).
    pub accept_unverified: bool,
    /// Uncertainty floor in the score denominator, dB.
    pub min_uncertainty_db: f64,
}

impl FloorAnomalyConfig {
    /// Defaults for a run.
    pub fn new(run: impl Into<String>) -> Self {
        Self {
            run: run.into(),
            accept_unverified: false,
            min_uncertainty_db: 0.05,
        }
    }
}

/// Lifecycle errors.
#[derive(Debug, thiserror::Error)]
pub enum AnomalyError {
    /// Repository.
    #[error(transparent)]
    Repo(#[from] RepoError),
    /// Bad run id or signal values.
    #[error("invalid: {0}")]
    Invalid(String),
}

/// What one signal did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LifecycleReport {
    /// Anomalies inserted (new parts and successors of superseded parts).
    pub opened: Vec<AnomalyId>,
    /// Closed anomalies re-opened (continued episodes).
    pub reopened: Vec<AnomalyId>,
    /// Anomalies resolved (not counting superseded ones).
    pub closed: Vec<AnomalyId>,
    /// Open anomalies moved to another episode (merge or split), still open.
    pub reparented: Vec<AnomalyId>,
    /// `(old, successor)` for each part whose extent changed.
    pub superseded: Vec<(AnomalyId, AnomalyId)>,
    /// Nothing written, and why (duplicate, class, unknown episode…).
    pub ignored: Option<&'static str>,
}

impl LifecycleReport {
    fn ignored(why: &'static str) -> Self {
        Self {
            ignored: Some(why),
            ..Self::default()
        }
    }

    fn nothing_if_empty(mut self, why: &'static str) -> Self {
        if self.opened.is_empty()
            && self.reopened.is_empty()
            && self.closed.is_empty()
            && self.reparented.is_empty()
            && self.superseded.is_empty()
        {
            self.ignored = Some(why);
        }
        self
    }
}

/// One anomaly of an episode.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EpisodePart {
    /// Anomaly.
    pub anomaly: AnomalyId,
    /// Its frequency extent.
    pub freq: FreqRange,
    /// Currently open.
    pub open: bool,
}

/// The key fields parsed from `baseline_ref`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpisodeKey {
    /// Tracker run.
    pub run: String,
    /// Segment.
    pub segment: u64,
    /// Episode.
    pub episode: u64,
    /// Onset, ns.
    pub onset_ns: i64,
}

/// `baseline_ref` for a part.
pub fn baseline_ref(run: &str, extent: &EpisodeExtent) -> String {
    format!(
        "{KEY_PREFIX};run={run};segment={};episode={};onset_ns={};baseline_dbfs_per_hz={:.2}",
        extent.segment,
        extent.episode,
        extent.onset.as_unix_nanos(),
        extent.baseline_dbfs_per_hz
    )
}

/// Parses a `baseline_ref` written by [`baseline_ref`] (`None` for anything else).
pub fn parse_baseline_ref(s: &str) -> Option<EpisodeKey> {
    let mut parts = s.split(';');
    if parts.next()? != KEY_PREFIX {
        return None;
    }
    let (mut run, mut segment, mut episode, mut onset) = (None, None, None, None);
    for p in parts {
        let (k, v) = p.split_once('=')?;
        match k {
            "run" => run = Some(v.to_owned()),
            "segment" => segment = v.parse().ok(),
            "episode" => episode = v.parse().ok(),
            "onset_ns" => onset = v.parse().ok(),
            _ => {}
        }
    }
    Some(EpisodeKey {
        run: run?,
        segment: segment?,
        episode: episode?,
        onset_ns: onset?,
    })
}

/// `(episode, primary)` from a status note carrying `episode=<id>;primary=<0|1>`.
fn parse_membership_note(note: &str) -> Option<(u64, bool)> {
    let (mut episode, mut primary) = (None, None);
    for p in note.split(';') {
        match p.split_once('=') {
            Some(("episode", v)) => episode = v.parse().ok(),
            Some(("primary", v)) => primary = Some(v == "1"),
            _ => {}
        }
    }
    Some((episode?, primary?))
}

fn valid_run(run: &str) -> bool {
    !run.is_empty()
        && run
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

/// A region query covering every anomaly.
fn everything() -> Region {
    Region::new(
        FreqRange::new(-1e30, 1e30),
        TimeRange::new(
            Timestamp::from_unix_nanos(i64::MIN / 2),
            Timestamp::from_unix_nanos(i64::MAX / 2),
        ),
    )
}

fn current_status(repo: &Repository, id: AnomalyId) -> Result<AnomalyStatus, RepoError> {
    Ok(repo
        .anomaly_status_history(id)?
        .last()
        .map_or(AnomalyStatus::Open, |c| c.status))
}

/// Width of the overlap of two ranges (0 when they only touch or are apart).
fn overlap_hz(a: &FreqRange, b: &FreqRange) -> f64 {
    (a.hi_hz.min(b.hi_hz) - a.lo_hz.max(b.lo_hz)).max(0.0)
}

/// Resolves open `noise-floor-rise` anomalies whose tracker run is not in `live_runs` (their end
/// can never be reported). Returns the closed ids.
pub fn close_orphaned(
    repo: &mut Repository,
    live_runs: &[&str],
    now: Timestamp,
) -> Result<Vec<AnomalyId>, AnomalyError> {
    let mut closed = Vec::new();
    for a in repo.anomalies_in_region(&everything())? {
        if a.kind != AnomalyKind::NoiseFloorRise || a.detector_version != DETECTOR_VERSION {
            continue;
        }
        let Some(key) = a.baseline_ref.as_deref().and_then(parse_baseline_ref) else {
            continue;
        };
        if live_runs.contains(&key.run.as_str())
            || current_status(repo, a.id)? != AnomalyStatus::Open
        {
            continue;
        }
        repo.append_anomaly_status(&AnomalyStatusChange {
            anomaly_id: a.id,
            status: AnomalyStatus::Resolved,
            t: now,
            note: Some(format!("tracker-run-ended;run={}", key.run)),
        })?;
        closed.push(a.id);
    }
    Ok(closed)
}

/// Copies `from`'s latest Explanations to `to` (`supersedes` = the copied one; ones whose
/// evidence is stale are skipped).
fn copy_explanations(
    repo: &mut Repository,
    from: AnomalyId,
    to: AnomalyId,
    t: Timestamp,
) -> Result<(), AnomalyError> {
    let explanations = repo.explanations_for_anomaly(from)?;
    let replaced: BTreeSet<ExplanationId> =
        explanations.iter().filter_map(|e| e.supersedes).collect();
    for e in explanations.iter().filter(|e| !replaced.contains(&e.id)) {
        let copy = Explanation {
            id: ExplanationId::new(),
            anomaly_ref: to,
            supersedes: Some(e.id),
            t,
            ..e.clone()
        };
        match repo.insert_explanation(&copy) {
            Ok(()) | Err(RepoError::StaleEvidence { .. }) => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

/// One anomaly of the run in the in-memory index.
#[derive(Clone, Debug)]
struct Part {
    anomaly: AnomalyId,
    /// `(episode, onset_ns)` of the `baseline_ref` key: identity for duplicate detection.
    origin: (u64, i64),
    /// Episode it belongs to now (after merges and splits).
    episode: u64,
    /// Follows its episode's extent.
    primary: bool,
    freq: FreqRange,
    open: bool,
}

/// Floor-episode → Anomaly lifecycle for one tracker run.
#[derive(Clone, Debug)]
pub struct FloorAnomalies {
    config: FloorAnomalyConfig,
    parts: Vec<Part>,
    /// `(episode, onset_ns)` of continuations already applied (re-opens), so a replayed
    /// continuation is a duplicate. Rebuilt from `episode-continued;onset_ns=…` status notes.
    continuations: BTreeSet<(u64, i64)>,
}

const CONTINUED_NOTE: &str = "episode-continued;onset_ns=";

impl FloorAnomalies {
    /// A lifecycle with no history (a new run).
    pub fn new(config: FloorAnomalyConfig) -> Result<Self, AnomalyError> {
        if !valid_run(&config.run) {
            return Err(AnomalyError::Invalid(format!(
                "run id {:?} must match [A-Za-z0-9._:-]+",
                config.run
            )));
        }
        if !(config.min_uncertainty_db.is_finite() && config.min_uncertainty_db > 0.0) {
            return Err(AnomalyError::Invalid(
                "min_uncertainty_db must be > 0".into(),
            ));
        }
        Ok(Self {
            config,
            parts: Vec::new(),
            continuations: BTreeSet::new(),
        })
    }

    /// A lifecycle for `config.run` rebuilt from the repository (process restart).
    pub fn resume(config: FloorAnomalyConfig, repo: &Repository) -> Result<Self, AnomalyError> {
        let mut me = Self::new(config)?;
        for a in repo.anomalies_in_region(&everything())? {
            if a.kind != AnomalyKind::NoiseFloorRise || a.detector_version != DETECTOR_VERSION {
                continue;
            }
            let Some(key) = a.baseline_ref.as_deref().and_then(parse_baseline_ref) else {
                continue;
            };
            if key.run != me.config.run {
                continue;
            }
            let history = repo.anomaly_status_history(a.id)?;
            let open = history
                .last()
                .is_none_or(|c| c.status == AnomalyStatus::Open);
            let (mut episode, mut primary) = (key.episode, true);
            for change in &history {
                let Some(note) = change.note.as_deref() else {
                    continue;
                };
                if let Some(onset) = note
                    .strip_prefix(CONTINUED_NOTE)
                    .and_then(|v| v.parse::<i64>().ok())
                {
                    me.continuations.insert((key.episode, onset));
                }
                if let Some((e, p)) = parse_membership_note(note) {
                    (episode, primary) = (e, p);
                }
            }
            me.parts.push(Part {
                anomaly: a.id,
                origin: (key.episode, key.onset_ns),
                episode,
                primary,
                freq: a.region.freq,
                open,
            });
        }
        me.sort();
        Ok(me)
    }

    fn sort(&mut self) {
        self.parts.sort_by(|a, b| {
            (a.episode, a.origin.1, a.anomaly).cmp(&(b.episode, b.origin.1, b.anomaly))
        });
    }

    /// Settings.
    pub fn config(&self) -> &FloorAnomalyConfig {
        &self.config
    }

    /// Parts currently belonging to an episode, by onset.
    pub fn episode_parts(&self, episode: u64) -> Vec<EpisodePart> {
        self.parts
            .iter()
            .filter(|p| p.episode == episode)
            .map(|p| EpisodePart {
                anomaly: p.anomaly,
                freq: p.freq,
                open: p.open,
            })
            .collect()
    }

    /// Open anomalies, by (episode, onset).
    pub fn open_anomalies(&self) -> Vec<AnomalyId> {
        self.parts
            .iter()
            .filter(|p| p.open)
            .map(|p| p.anomaly)
            .collect()
    }

    fn accepts(&self, class: EpisodeClass) -> bool {
        match class {
            EpisodeClass::NoiseLike => true,
            EpisodeClass::Unverified => self.config.accept_unverified,
            EpisodeClass::Structured => false,
        }
    }

    /// Maps and applies a floor-tracker event.
    pub fn on_floor_event(
        &mut self,
        repo: &mut Repository,
        event: &FloorEvent,
    ) -> Result<LifecycleReport, AnomalyError> {
        match signal_from_floor_event(event) {
            Some(signal) => self.apply(repo, &signal),
            None => Ok(LifecycleReport::ignored("unmapped floor event")),
        }
    }

    /// Applies one signal.
    pub fn apply(
        &mut self,
        repo: &mut Repository,
        signal: &EpisodeSignal,
    ) -> Result<LifecycleReport, AnomalyError> {
        let report = match signal {
            EpisodeSignal::Opened {
                extent,
                continued,
                split_from: Some(parent),
            } => {
                let _ = continued;
                self.split(repo, extent, *parent)
            }
            EpisodeSignal::Opened {
                extent, continued, ..
            } => self.open(repo, extent, *continued),
            EpisodeSignal::Extended(extent) => self.extend(repo, extent),
            EpisodeSignal::Updated {
                episode,
                t,
                f_lo_hz,
                f_hi_hz,
            } => {
                let current = FreqRange::new(f_lo_hz.min(*f_hi_hz), f_lo_hz.max(*f_hi_hz));
                self.shrink(repo, *episode, *t, current)
            }
            EpisodeSignal::Closed {
                episode,
                t,
                reason: CloseReason::Merged,
                merged_into: Some(into),
                ..
            } => self.reparent(repo, *episode, *into, *t),
            EpisodeSignal::Closed {
                episode,
                t,
                reason,
                duration_s,
                ..
            } => {
                let note = format!(
                    "episode-end;reason={};duration_s={duration_s:.3}",
                    match reason {
                        CloseReason::Returned => "returned",
                        CloseReason::Reset => "reset",
                        CloseReason::Merged => "merged",
                        CloseReason::Rebaselined => "rebaselined",
                    }
                );
                self.close_where(repo, *t, &note, |p| p.episode == *episode)
            }
            EpisodeSignal::Unknown { episode, t } => {
                self.close_where(repo, *t, "episode-state-unknown", |p| {
                    episode.is_none_or(|e| p.episode == e)
                })
            }
            EpisodeSignal::Fell {
                t,
                f_lo_hz,
                f_hi_hz,
                interrupted,
            } => {
                let fall = FreqRange::new(f_lo_hz.min(*f_hi_hz), f_lo_hz.max(*f_hi_hz));
                let note = if *interrupted {
                    "floor-fall;interrupted"
                } else {
                    "floor-fall"
                };
                self.close_where(repo, *t, note, |p| p.freq.overlaps(&fall))
            }
        }?;
        self.sort();
        Ok(report)
    }

    fn validate(extent: &EpisodeExtent) -> Result<(), AnomalyError> {
        if extent.f_lo_hz.is_finite()
            && extent.f_hi_hz.is_finite()
            && extent.f_hi_hz >= extent.f_lo_hz
            && extent.confirmed >= extent.onset
            && extent.step_db.is_finite()
        {
            Ok(())
        } else {
            Err(AnomalyError::Invalid(format!("episode extent {extent:?}")))
        }
    }

    /// Inserts a part for `extent`.
    fn insert_part(
        &mut self,
        repo: &mut Repository,
        extent: &EpisodeExtent,
        primary: bool,
    ) -> Result<AnomalyId, AnomalyError> {
        Self::validate(extent)?;
        let uncertainty = if extent.step_uncertainty_db.is_finite() {
            extent
                .step_uncertainty_db
                .max(self.config.min_uncertainty_db)
        } else {
            self.config.min_uncertainty_db
        };
        let freq = FreqRange::new(extent.f_lo_hz, extent.f_hi_hz);
        let anomaly = Anomaly {
            id: AnomalyId::new(),
            kind: AnomalyKind::NoiseFloorRise,
            subject: AnomalySubject::Region,
            region: Region::new(freq, TimeRange::new(extent.onset, extent.confirmed)),
            score: extent.step_db / uncertainty,
            baseline_ref: Some(baseline_ref(&self.config.run, extent)),
            t: extent.confirmed,
            detector_version: DETECTOR_VERSION.into(),
        };
        repo.insert_anomaly(&anomaly)?;
        self.parts.push(Part {
            anomaly: anomaly.id,
            origin: (extent.episode, extent.onset.as_unix_nanos()),
            episode: extent.episode,
            primary,
            freq,
            open: true,
        });
        Ok(anomaly.id)
    }

    fn open(
        &mut self,
        repo: &mut Repository,
        extent: &EpisodeExtent,
        continued: bool,
    ) -> Result<LifecycleReport, AnomalyError> {
        if !self.accepts(extent.class) {
            return Ok(LifecycleReport::ignored(
                "episode class does not open an anomaly",
            ));
        }
        let key = (extent.episode, extent.onset.as_unix_nanos());
        if self.continuations.contains(&key) {
            return Ok(LifecycleReport::ignored("duplicate episode continuation"));
        }
        if continued {
            let closed: Vec<usize> = (0..self.parts.len())
                .filter(|&i| self.parts[i].episode == extent.episode && !self.parts[i].open)
                .collect();
            if !closed.is_empty() {
                let mut report = LifecycleReport::default();
                for i in closed {
                    let part = &mut self.parts[i];
                    repo.append_anomaly_status(&AnomalyStatusChange {
                        anomaly_id: part.anomaly,
                        status: AnomalyStatus::Open,
                        t: extent.confirmed,
                        note: Some(format!("{CONTINUED_NOTE}{}", key.1)),
                    })?;
                    part.open = true;
                    report.reopened.push(part.anomaly);
                }
                self.continuations.insert(key);
                return Ok(report);
            }
        }
        if self.parts.iter().any(|p| p.origin == key) {
            return Ok(LifecycleReport::ignored("duplicate episode onset"));
        }
        let id = self.insert_part(repo, extent, true)?;
        Ok(LifecycleReport {
            opened: vec![id],
            ..LifecycleReport::default()
        })
    }

    /// Replaces part `i`'s anomaly by a successor with frequency extent `freq` (see the module
    /// docs), copying its latest explanations.
    fn supersede(
        &mut self,
        repo: &mut Repository,
        i: usize,
        freq: FreqRange,
        t: Timestamp,
        report: &mut LifecycleReport,
    ) -> Result<(), AnomalyError> {
        let old = repo.anomaly(self.parts[i].anomaly)?;
        let end = if t > old.region.time.end {
            t
        } else {
            old.region.time.end
        };
        let successor = Anomaly {
            id: AnomalyId::new(),
            region: Region::new(freq, TimeRange::new(old.region.time.start, end)),
            t: if t > old.t { t } else { old.t },
            ..old.clone()
        };
        repo.insert_anomaly(&successor)?;
        let part = &self.parts[i];
        repo.append_anomaly_status(&AnomalyStatusChange {
            anomaly_id: successor.id,
            status: AnomalyStatus::Open,
            t: successor.t,
            note: Some(format!(
                "supersedes={};episode={};primary={}",
                old.id,
                part.episode,
                u8::from(part.primary)
            )),
        })?;
        repo.append_anomaly_status(&AnomalyStatusChange {
            anomaly_id: old.id,
            status: AnomalyStatus::Resolved,
            t: successor.t,
            note: Some(format!("superseded;by={}", successor.id)),
        })?;
        copy_explanations(repo, old.id, successor.id, successor.t)?;
        let part = &mut self.parts[i];
        part.anomaly = successor.id;
        part.freq = freq;
        report.superseded.push((old.id, successor.id));
        report.opened.push(successor.id);
        Ok(())
    }

    fn extend(
        &mut self,
        repo: &mut Repository,
        extent: &EpisodeExtent,
    ) -> Result<LifecycleReport, AnomalyError> {
        if !self.accepts(extent.class) {
            return Ok(LifecycleReport::ignored(
                "episode class does not open an anomaly",
            ));
        }
        Self::validate(extent)?;
        let added = FreqRange::new(extent.f_lo_hz, extent.f_hi_hz);
        let primary = self
            .parts
            .iter()
            .position(|p| p.open && p.primary && p.episode == extent.episode);
        let mut report = LifecycleReport::default();
        match primary {
            Some(i) => {
                let f = self.parts[i].freq;
                let hull = FreqRange::new(f.lo_hz.min(added.lo_hz), f.hi_hz.max(added.hi_hz));
                if hull != f {
                    self.supersede(repo, i, hull, extent.confirmed, &mut report)?;
                }
                self.absorb_into_primary(repo, extent.episode, extent.confirmed, &mut report)?;
            }
            None => {
                let key = (extent.episode, extent.onset.as_unix_nanos());
                if !self.parts.iter().any(|p| p.origin == key) {
                    report.opened.push(self.insert_part(repo, extent, true)?);
                }
            }
        }
        Ok(report.nothing_if_empty("extent already covered"))
    }

    fn shrink(
        &mut self,
        repo: &mut Repository,
        episode: u64,
        t: Timestamp,
        current: FreqRange,
    ) -> Result<LifecycleReport, AnomalyError> {
        let mut report = LifecycleReport::default();
        for i in 0..self.parts.len() {
            let p = &self.parts[i];
            if !p.open || p.episode != episode {
                continue;
            }
            if overlap_hz(&p.freq, &current) <= 0.0 {
                repo.append_anomaly_status(&AnomalyStatusChange {
                    anomaly_id: p.anomaly,
                    status: AnomalyStatus::Resolved,
                    t,
                    note: Some("extent-returned".into()),
                })?;
                report.closed.push(p.anomaly);
                self.parts[i].open = false;
                continue;
            }
            let clipped = FreqRange::new(
                p.freq.lo_hz.max(current.lo_hz),
                p.freq.hi_hz.min(current.hi_hz),
            );
            if clipped != p.freq {
                self.supersede(repo, i, clipped, t, &mut report)?;
            }
        }
        Ok(report.nothing_if_empty("no open anomaly matches"))
    }

    fn reparent(
        &mut self,
        repo: &mut Repository,
        episode: u64,
        into: u64,
        t: Timestamp,
    ) -> Result<LifecycleReport, AnomalyError> {
        let mut report = LifecycleReport::default();
        for p in self
            .parts
            .iter_mut()
            .filter(|p| p.open && p.episode == episode)
        {
            repo.append_anomaly_status(&AnomalyStatusChange {
                anomaly_id: p.anomaly,
                status: AnomalyStatus::Open,
                t,
                note: Some(format!(
                    "episode-merged;from={episode};episode={into};primary=0"
                )),
            })?;
            p.episode = into;
            p.primary = false;
            report.reparented.push(p.anomaly);
        }
        Ok(report.nothing_if_empty("no open anomaly matches"))
    }

    fn split(
        &mut self,
        repo: &mut Repository,
        extent: &EpisodeExtent,
        parent: u64,
    ) -> Result<LifecycleReport, AnomalyError> {
        Self::validate(extent)?;
        if self.parts.iter().any(|p| p.episode == extent.episode) {
            return Ok(LifecycleReport::ignored("duplicate episode split"));
        }
        let target = FreqRange::new(extent.f_lo_hz, extent.f_hi_hz);
        let moving: Vec<usize> = (0..self.parts.len())
            .filter(|&i| {
                let p = &self.parts[i];
                p.open
                    && p.episode == parent
                    && overlap_hz(&p.freq, &target) >= 0.5 * p.freq.width_hz()
                    && overlap_hz(&p.freq, &target) > 0.0
            })
            .collect();
        let mut report = LifecycleReport::default();
        // The parent keeps the rest: clip its other open parts off the split extent when it lies
        // at one of their ends (the parent's Update clips the rest), so open parts never overlap.
        for i in 0..self.parts.len() {
            let p = &self.parts[i];
            if !p.open
                || p.episode != parent
                || moving.contains(&i)
                || overlap_hz(&p.freq, &target) <= 0.0
            {
                continue;
            }
            let rest = if target.lo_hz <= p.freq.lo_hz {
                FreqRange::new(target.hi_hz, p.freq.hi_hz)
            } else if target.hi_hz >= p.freq.hi_hz {
                FreqRange::new(p.freq.lo_hz, target.lo_hz)
            } else {
                continue;
            };
            if rest.width_hz() > 0.0 {
                self.supersede(repo, i, rest, extent.confirmed, &mut report)?;
            }
        }
        if moving.is_empty() {
            if self.accepts(extent.class) {
                report.opened.push(self.insert_part(repo, extent, true)?);
            }
            return Ok(report.nothing_if_empty("episode class does not open an anomaly"));
        }
        let widest = *moving
            .iter()
            .max_by(|&&a, &&b| {
                self.parts[a]
                    .freq
                    .width_hz()
                    .total_cmp(&self.parts[b].freq.width_hz())
            })
            .expect("non-empty");
        for &i in &moving {
            let primary = i == widest;
            let p = &mut self.parts[i];
            repo.append_anomaly_status(&AnomalyStatusChange {
                anomaly_id: p.anomaly,
                status: AnomalyStatus::Open,
                t: extent.confirmed,
                note: Some(format!(
                    "episode-split;from={parent};episode={};primary={}",
                    extent.episode,
                    u8::from(primary)
                )),
            })?;
            p.episode = extent.episode;
            p.primary = primary;
            report.reparented.push(p.anomaly);
            let clipped = FreqRange::new(
                p.freq.lo_hz.max(target.lo_hz),
                p.freq.hi_hz.min(target.hi_hz),
            );
            if clipped != p.freq {
                self.supersede(repo, i, clipped, extent.confirmed, &mut report)?;
            }
        }
        self.absorb_into_primary(repo, extent.episode, extent.confirmed, &mut report)?;
        Ok(report)
    }

    /// Absorbs the episode's other open parts that overlap its open primary: the primary grows
    /// to their hull (inside the episode's extent, since every part is), each absorbed part's
    /// latest Explanations are copied to it, and the part is resolved `absorbed;by=<primary>`
    /// (reported as superseded by the primary). Open anomalies of one episode never overlap.
    fn absorb_into_primary(
        &mut self,
        repo: &mut Repository,
        episode: u64,
        t: Timestamp,
        report: &mut LifecycleReport,
    ) -> Result<(), AnomalyError> {
        let Some(pi) = self
            .parts
            .iter()
            .position(|p| p.open && p.primary && p.episode == episode)
        else {
            return Ok(());
        };
        let mut hull = self.parts[pi].freq;
        let mut absorbed: Vec<usize> = Vec::new();
        loop {
            let before = absorbed.len();
            for (j, p) in self.parts.iter().enumerate() {
                if j != pi
                    && p.open
                    && p.episode == episode
                    && !absorbed.contains(&j)
                    && overlap_hz(&p.freq, &hull) > 0.0
                {
                    hull =
                        FreqRange::new(hull.lo_hz.min(p.freq.lo_hz), hull.hi_hz.max(p.freq.hi_hz));
                    absorbed.push(j);
                }
            }
            if absorbed.len() == before {
                break;
            }
        }
        if absorbed.is_empty() {
            return Ok(());
        }
        if hull != self.parts[pi].freq {
            self.supersede(repo, pi, hull, t, report)?;
        }
        let into = self.parts[pi].anomaly;
        for j in absorbed {
            let from = self.parts[j].anomaly;
            copy_explanations(repo, from, into, t)?;
            repo.append_anomaly_status(&AnomalyStatusChange {
                anomaly_id: from,
                status: AnomalyStatus::Resolved,
                t,
                note: Some(format!("absorbed;by={into}")),
            })?;
            self.parts[j].open = false;
            report.superseded.push((from, into));
        }
        Ok(())
    }

    fn close_where(
        &mut self,
        repo: &mut Repository,
        t: Timestamp,
        note: &str,
        mut matches: impl FnMut(&Part) -> bool,
    ) -> Result<LifecycleReport, AnomalyError> {
        let mut report = LifecycleReport::default();
        for part in self.parts.iter_mut() {
            if !part.open || !matches(part) {
                continue;
            }
            repo.append_anomaly_status(&AnomalyStatusChange {
                anomaly_id: part.anomaly,
                status: AnomalyStatus::Resolved,
                t,
                note: Some(note.to_owned()),
            })?;
            part.open = false;
            report.closed.push(part.anomaly);
        }
        if report.closed.is_empty() {
            report.ignored = Some("no open anomaly matches");
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests;
