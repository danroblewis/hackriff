//! Anomaly lifecycle from noise-floor episodes (C08 → C30; AWARE-006).
//!
//! The lifecycle consumes [`EpisodeSignal`], an internal adapter over the floor tracker's events.
//! [`signal_from_floor_event`] is the **only** place that knows `hk_dsp::floor::FloorEvent`, so a
//! change to the tracker's event model (Extend / Update / interrupted Fall / Rise(continued) /
//! Unknown) changes that mapping, not the lifecycle.
//!
//! # Rules
//!
//! - **Opened** (`Rise`) of an accepted class opens an `Anomaly(noise-floor-rise)` for its extent.
//!   Accepted: `NoiseLike` (and `Unverified` when configured). Structured episodes never open one;
//!   the tracker may emit them, so the class is filtered here.
//! - **Extended**: the added extent has its own onset, so it opens its own Anomaly (a later onset
//!   is a new local deviation worth correlating). Episode parts close together.
//! - **Updated** (extent shrank): open parts that no longer overlap the extent are resolved
//!   (`extent-returned`). Anomaly rows are append-only, so a shrinking part keeps its region.
//! - **Closed** (`End`, reason returned or reset): every open part of the episode is resolved.
//! - **Unknown** (tracker lost the episode, e.g. after a reset): open parts of that episode (or of
//!   the whole run) are resolved with `episode-state-unknown`; a later `Opened { continued }`
//!   re-opens them.
//! - **Fell** (a floor fall, possibly interrupted): open parts overlapping the fall are resolved
//!   (`floor-fall`), since a fall below the baseline means the rise is over.
//!
//! # Identity, duplicates, restarts
//!
//! A part is keyed by `(run, episode, onset)`; `run` names one tracker instance (episode ids are
//! unique per tracker only). The key is written into `Anomaly::baseline_ref` as
//! `floor-episode:v1;run=…;segment=…;episode=…;onset_ns=…;baseline_dbfs_per_hz=…`, the repository
//! is the source of truth, and [`FloorAnomalies::resume`] rebuilds the in-memory index from it. A
//! replayed Rise/Extend for a known key, or a replayed End of a closed episode, writes nothing. An
//! anomaly left open by a tracker run that no longer exists is closed by [`close_orphaned`].
//!
//! Status changes go to the append-only history: `open` at insert, then `resolved` with a note.
//! The anomaly's region time is `[onset, confirmation]`; the end time is the `resolved` entry.
//! Not for the real-time path: each signal is a SQLite transaction.

use std::collections::{BTreeMap, BTreeSet};

use hk_dsp::floor::{EndReason, FloorChangeClass, FloorEvent, FloorEventKind};
use hk_model::{
    Anomaly, AnomalyId, AnomalyKind, AnomalyStatus, AnomalyStatusChange, AnomalySubject, FreqRange,
    Region, RepoError, Repository, TimeRange, Timestamp,
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
}

/// The lifecycle's view of floor-tracker output.
#[derive(Clone, Debug, PartialEq)]
pub enum EpisodeSignal {
    /// A confirmed rise. `continued`: the tracker re-opened an episode after a reset.
    Opened {
        /// Extent.
        extent: EpisodeExtent,
        /// Continuation after a reset.
        continued: bool,
    },
    /// Regions merged: the added extent with its own onset.
    Extended(EpisodeExtent),
    /// The episode extent shrank as blocks returned.
    Updated {
        /// Episode.
        episode: u64,
        /// When.
        t: Timestamp,
        /// Remaining lower edge, Hz.
        f_lo_hz: f64,
        /// Remaining upper edge, Hz.
        f_hi_hz: f64,
    },
    /// The episode ended.
    Closed {
        /// Episode.
        episode: u64,
        /// When (first returned frame, or the resetting frame).
        t: Timestamp,
        /// Why.
        reason: CloseReason,
        /// Episode length, s.
        duration_s: f64,
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
        /// The fall was interrupted (adopted silently by the tracker).
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
    Some(match e.kind {
        FloorEventKind::Rise => EpisodeSignal::Opened {
            extent: EpisodeExtent {
                episode: e.episode,
                segment: e.segment,
                class,
                onset: e.onset_t.host_time,
                confirmed: e.confirmed_t.host_time,
                f_lo_hz: e.f_lo_hz,
                f_hi_hz: e.f_hi_hz,
                step_db: f64::from(e.step_db),
                step_uncertainty_db: f64::from(e.step_uncertainty_db),
                baseline_dbfs_per_hz: f64::from(e.baseline_dbfs_per_hz),
            },
            continued: false,
        },
        FloorEventKind::End => EpisodeSignal::Closed {
            episode: e.episode,
            t: e.onset_t.host_time,
            reason: match e.end_reason {
                Some(EndReason::Reset) => CloseReason::Reset,
                Some(EndReason::Returned) | None => CloseReason::Returned,
            },
            duration_s: e.duration_s,
        },
        FloorEventKind::Fall => EpisodeSignal::Fell {
            t: e.confirmed_t.host_time,
            f_lo_hz: e.f_lo_hz,
            f_hi_hz: e.f_hi_hz,
            interrupted: false,
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
    /// Anomalies inserted.
    pub opened: Vec<AnomalyId>,
    /// Closed anomalies re-opened (continued episodes).
    pub reopened: Vec<AnomalyId>,
    /// Anomalies resolved.
    pub closed: Vec<AnomalyId>,
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

/// Floor-episode → Anomaly lifecycle for one tracker run.
#[derive(Clone, Debug)]
pub struct FloorAnomalies {
    config: FloorAnomalyConfig,
    /// `(episode, onset_ns)` → part.
    parts: BTreeMap<(u64, i64), EpisodePart>,
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
            parts: BTreeMap::new(),
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
            for change in &history {
                if let Some(onset) = change
                    .note
                    .as_deref()
                    .and_then(|n| n.strip_prefix(CONTINUED_NOTE))
                    .and_then(|v| v.parse::<i64>().ok())
                {
                    me.continuations.insert((key.episode, onset));
                }
            }
            me.parts.insert(
                (key.episode, key.onset_ns),
                EpisodePart {
                    anomaly: a.id,
                    freq: a.region.freq,
                    open,
                },
            );
        }
        Ok(me)
    }

    /// Settings.
    pub fn config(&self) -> &FloorAnomalyConfig {
        &self.config
    }

    /// Parts of an episode, by onset.
    pub fn episode_parts(&self, episode: u64) -> Vec<EpisodePart> {
        self.parts
            .range((episode, i64::MIN)..=(episode, i64::MAX))
            .map(|(_, p)| *p)
            .collect()
    }

    /// Open anomalies, by (episode, onset).
    pub fn open_anomalies(&self) -> Vec<AnomalyId> {
        self.parts
            .values()
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
        match signal {
            EpisodeSignal::Opened { extent, continued } => self.open(repo, extent, *continued),
            EpisodeSignal::Extended(extent) => self.open(repo, extent, false),
            EpisodeSignal::Updated {
                episode,
                t,
                f_lo_hz,
                f_hi_hz,
            } => {
                let remaining = FreqRange::new(f_lo_hz.min(*f_hi_hz), f_lo_hz.max(*f_hi_hz));
                self.close_where(repo, *t, "extent-returned", |k, p| {
                    k.0 == *episode && !p.freq.overlaps(&remaining)
                })
            }
            EpisodeSignal::Closed {
                episode,
                t,
                reason,
                duration_s,
            } => {
                let note = format!(
                    "episode-end;reason={};duration_s={duration_s:.3}",
                    match reason {
                        CloseReason::Returned => "returned",
                        CloseReason::Reset => "reset",
                    }
                );
                self.close_where(repo, *t, &note, |k, _| k.0 == *episode)
            }
            EpisodeSignal::Unknown { episode, t } => {
                self.close_where(repo, *t, "episode-state-unknown", |k, _| {
                    episode.is_none_or(|e| k.0 == e)
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
                self.close_where(repo, *t, note, |_, p| p.freq.overlaps(&fall))
            }
        }
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
            let closed: Vec<(u64, i64)> = self
                .parts
                .range((extent.episode, i64::MIN)..=(extent.episode, i64::MAX))
                .filter(|(_, p)| !p.open)
                .map(|(k, _)| *k)
                .collect();
            if !closed.is_empty() {
                let mut report = LifecycleReport::default();
                for k in closed {
                    let part = self.parts.get_mut(&k).expect("key from range");
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
        if self.parts.contains_key(&key) {
            return Ok(LifecycleReport::ignored("duplicate episode onset"));
        }
        if !(extent.f_lo_hz.is_finite()
            && extent.f_hi_hz.is_finite()
            && extent.f_hi_hz >= extent.f_lo_hz
            && extent.confirmed >= extent.onset
            && extent.step_db.is_finite())
        {
            return Err(AnomalyError::Invalid(format!("episode extent {extent:?}")));
        }
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
        self.parts.insert(
            key,
            EpisodePart {
                anomaly: anomaly.id,
                freq,
                open: true,
            },
        );
        Ok(LifecycleReport {
            opened: vec![anomaly.id],
            ..LifecycleReport::default()
        })
    }

    fn close_where(
        &mut self,
        repo: &mut Repository,
        t: Timestamp,
        note: &str,
        mut matches: impl FnMut(&(u64, i64), &EpisodePart) -> bool,
    ) -> Result<LifecycleReport, AnomalyError> {
        let mut report = LifecycleReport::default();
        for (key, part) in self.parts.iter_mut() {
            if !part.open || !matches(key, part) {
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
