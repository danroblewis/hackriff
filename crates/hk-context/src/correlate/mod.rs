//! Event correlation: Anomaly × cached ExternalEvents → ranked Explanations (C30; AWARE-006).
//!
//! # Rule `gnss-cell` (this slice)
//!
//! Applies to `noise-floor-rise` anomalies and the novelty alarm kinds ([`CORRELATED_KINDS`],
//! T-122), and events from [`CorrelatorConfig::sources`]
//! (default `gpsjam`). A candidate must pass every gate; its score is a product, so any factor at
//! 0 removes it:
//!
//! ```text
//! score = prior · s_time · s_geo · s_band · s_mag
//! s_time = 1 − gap / W            (gap: separation of anomaly and event spans, 0 if they overlap;
//!                                   W = time_window_s; with W = 0, 1 only on overlap)
//! s_geo  = 1 − d / R              (d: km from the device site to the event region, 0 inside;
//!                                   R = geo_radius_km; a Global event scores 1)
//! s_band = |anomaly band ∩ event bands| / |anomaly band|   (1 for a zero-width anomaly inside)
//! s_mag  = clamp(0.5 + 0.5·(percent_bad − 2)/8, 0.5, 1)    (0.5 without percent_bad)
//! ```
//!
//! Candidates with `score < min_score` (default 0.2) are dropped: **nothing matching means no
//! Explanation**, never a low-score one. Ranking: score (desc), event start, source, native id.
//! No device site → only `Global` events can pass (no geometry, no claim). Not yet implemented
//! (C30 card): the base-rate correction and self-inflicted hypotheses.
//!
//! # Output
//!
//! One Explanation per surviving candidate (at most `max_explanations`): cause = the event,
//! `correlation_type` time-coincidence, evidence = the event pinned to its payload hash, the
//! anomaly region, and the numeric facts (`time_gap_s`, `distance_km`, `band_overlap`, each score
//! factor, `rule_prior`, `source_stale`, `cache_age_s`, site). `t` is the evaluation time passed
//! in, so a frozen cache gives identical explanations (ids aside).
//!
//! **Stale cache** (event past `valid_until`, or its feed stale): the candidate still counts;
//! the Explanation is `provisional` with `source_stale = 1`.
//!
//! **Re-correlation** is append-only and idempotent: a candidate whose latest explanation already
//! pins the same payload hash with the same score and provisional flag writes nothing; otherwise a
//! new row `supersedes` it. A latest explanation whose pinned hash no longer matches the cache is
//! reported as [`StaleEvidence`]. A payload revised between ranking and writing makes the
//! repository refuse the write (`RepoError::StaleEvidence`); [`Correlator::correlate`] reports it
//! and re-ranks (up to [`MAX_ATTEMPTS`]).

use std::collections::{BTreeMap, BTreeSet};

use hk_model::{
    Anomaly, AnomalyId, AnomalyKind, Cause, ContentHash, CorrelationType, Evidence, Explanation,
    ExplanationId, ExternalEvent, ExternalEventId, FreqRange, RepoError, Repository, TimeRange,
    Timestamp,
};

use crate::feeds::{FeedCache, FeedError, FeedState};
use crate::geo::{Site, distance_km};

/// Rule set id and version written to `Explanation::rule_version`.
pub const RULE_VERSION: &str = "hk-context.gnss-cell@1";
/// Anomaly kinds the rules apply to: floor rises (C08) and the C12 novelty alarm kinds (T-122,
/// ADR-0012 §7.4 stages 2–3; the band gate keeps an event from explaining an anomaly outside its
/// bands). Open-set `novelty` has no rule.
pub const CORRELATED_KINDS: [AnomalyKind; 6] = [
    AnomalyKind::NoiseFloorRise,
    AnomalyKind::LevelAboveBaseline,
    AnomalyKind::BusierThanBaseline,
    AnomalyKind::QuieterThanBaseline,
    AnomalyKind::ChangePoint,
    AnomalyKind::NewEmitter,
];
/// Ranking/writing attempts before [`CorrelateError::Unsettled`].
pub const MAX_ATTEMPTS: usize = 3;

/// Correlator settings.
#[derive(Clone, Debug, PartialEq)]
pub struct CorrelatorConfig {
    /// Lag window `W` around the anomaly, s.
    pub time_window_s: f64,
    /// Geometry radius `R`, km.
    pub geo_radius_km: f64,
    /// Display threshold: lower scores write nothing.
    pub min_score: f64,
    /// Most explanations written per anomaly.
    pub max_explanations: usize,
    /// Rule prior.
    pub rule_prior: f64,
    /// Event sources the rule reads.
    pub sources: Vec<String>,
}

impl Default for CorrelatorConfig {
    fn default() -> Self {
        Self {
            time_window_s: 7200.0,
            geo_radius_km: 50.0,
            min_score: 0.2,
            max_explanations: 5,
            rule_prior: 0.9,
            sources: vec![crate::feeds::gpsjam::SOURCE.to_owned()],
        }
    }
}

/// Score factors of a candidate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScoreParts {
    /// Rule prior.
    pub prior: f64,
    /// Time factor.
    pub time: f64,
    /// Geometry factor.
    pub geo: f64,
    /// Band-relevance factor.
    pub band: f64,
    /// Magnitude factor.
    pub magnitude: f64,
}

/// A ranked candidate cause.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    /// The cached event.
    pub event: ExternalEvent,
    /// Its payload hash (pinned by the evidence).
    pub payload_hash: ContentHash,
    /// Combined score, 0–1.
    pub score: f64,
    /// Factors.
    pub parts: ScoreParts,
    /// Separation of anomaly and event spans, s.
    pub time_gap_s: f64,
    /// Site to event region, km.
    pub distance_km: f64,
    /// Band overlap fraction.
    pub band_overlap: f64,
    /// Source stale at evaluation time.
    pub stale: bool,
    /// Event cache age at evaluation time, s.
    pub cache_age_s: f64,
}

/// An existing explanation whose pinned payload no longer matches the cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StaleEvidence {
    /// Explanation holding the pin.
    pub explanation: Option<ExplanationId>,
    /// Event.
    pub event: ExternalEventId,
    /// Pinned hash.
    pub pinned: ContentHash,
    /// Hash now cached.
    pub current: ContentHash,
}

/// Result of [`Correlator::correlate`].
#[derive(Clone, Debug, PartialEq)]
pub struct CorrelationOutcome {
    /// Anomaly.
    pub anomaly: AnomalyId,
    /// Ranked candidates of the final attempt.
    pub candidates: Vec<Candidate>,
    /// Explanations written, best first.
    pub written: Vec<Explanation>,
    /// Latest explanations left as they are (already current).
    pub unchanged: Vec<ExplanationId>,
    /// Pins found or refused as stale.
    pub stale_evidence: Vec<StaleEvidence>,
    /// Attempts used.
    pub attempts: usize,
}

/// Correlation errors.
#[derive(Debug, thiserror::Error)]
pub enum CorrelateError {
    /// Repository (a refused stale pin is `RepoError::StaleEvidence`).
    #[error(transparent)]
    Repo(#[from] RepoError),
    /// Feed state.
    #[error(transparent)]
    Feed(#[from] FeedError),
    /// Payload hashing.
    #[error("payload hash: {0}")]
    Json(#[from] serde_json::Error),
    /// Bad settings or inputs.
    #[error("invalid: {0}")]
    Invalid(String),
    /// The cache kept changing under the correlator.
    #[error("cache kept changing: gave up after {0} attempts")]
    Unsettled(usize),
}

fn gap_ns(a: &TimeRange, b: &TimeRange) -> i64 {
    if a.overlaps(b) {
        0
    } else if a.end < b.start {
        b.start
            .as_unix_nanos()
            .saturating_sub(a.end.as_unix_nanos())
    } else {
        a.start
            .as_unix_nanos()
            .saturating_sub(b.end.as_unix_nanos())
    }
}

fn band_overlap(anomaly: &FreqRange, bands: &[FreqRange]) -> f64 {
    let width = anomaly.width_hz();
    if width <= 0.0 {
        return if bands.iter().any(|b| b.overlaps(anomaly)) {
            1.0
        } else {
            0.0
        };
    }
    let covered: f64 = bands
        .iter()
        .map(|b| (anomaly.hi_hz.min(b.hi_hz) - anomaly.lo_hz.max(b.lo_hz)).max(0.0))
        .sum();
    (covered / width).clamp(0.0, 1.0)
}

fn magnitude(event: &ExternalEvent) -> f64 {
    event
        .payload
        .get("percent_bad")
        .and_then(serde_json::Value::as_f64)
        .map_or(0.5, |p| (0.5 + 0.5 * (p - 2.0) / 8.0).clamp(0.5, 1.0))
}

/// The C30 correlator.
#[derive(Clone, Debug, Default)]
pub struct Correlator {
    /// Settings.
    pub config: CorrelatorConfig,
}

impl Correlator {
    /// A correlator with `config` (validated).
    pub fn new(config: CorrelatorConfig) -> Result<Self, CorrelateError> {
        let c = &config;
        let ok = c.time_window_s.is_finite()
            && c.time_window_s >= 0.0
            && c.geo_radius_km.is_finite()
            && c.geo_radius_km >= 0.0
            && c.min_score.is_finite()
            && c.min_score > 0.0
            && c.rule_prior.is_finite()
            && c.rule_prior > 0.0
            && c.rule_prior <= 1.0
            && c.max_explanations > 0;
        if ok {
            Ok(Self { config })
        } else {
            Err(CorrelateError::Invalid(format!(
                "correlator config {config:?}"
            )))
        }
    }

    /// The anomaly span widened by the lag window (the event query window).
    pub fn query_window(&self, anomaly: &Anomaly) -> TimeRange {
        let w = (self.config.time_window_s * 1e9) as i64;
        TimeRange::new(
            anomaly.region.time.start.saturating_add_nanos(-w),
            anomaly.region.time.end.saturating_add_nanos(w),
        )
    }

    /// Ranks candidate causes (pure; deterministic for the same inputs).
    pub fn rank(
        &self,
        anomaly: &Anomaly,
        site: Option<&Site>,
        events: &[ExternalEvent],
        feed_states: &BTreeMap<String, FeedState>,
        now: Timestamp,
    ) -> Result<Vec<Candidate>, CorrelateError> {
        if !CORRELATED_KINDS.contains(&anomaly.kind) {
            return Ok(Vec::new());
        }
        if let Some(s) = site {
            if !s.is_valid() {
                return Err(CorrelateError::Invalid(format!("site {s:?}")));
            }
        }
        let c = &self.config;
        let mut out = Vec::new();
        for event in events {
            if !c.sources.contains(&event.source) {
                continue;
            }
            let gap_s = gap_ns(&anomaly.region.time, &event.time) as f64 / 1e9;
            let s_time = if gap_s == 0.0 {
                1.0
            } else if c.time_window_s > 0.0 {
                (1.0 - gap_s / c.time_window_s).max(0.0)
            } else {
                0.0
            };
            let d_km = match (site, &event.geo) {
                (_, hk_model::Geo::Global) => Some(0.0),
                (Some(s), geo) => distance_km(s, geo),
                (None, _) => None,
            };
            let Some(d_km) = d_km else { continue };
            let s_geo = if d_km == 0.0 {
                1.0
            } else if c.geo_radius_km > 0.0 {
                (1.0 - d_km / c.geo_radius_km).max(0.0)
            } else {
                0.0
            };
            let overlap = band_overlap(&anomaly.region.freq, &event.freq);
            let parts = ScoreParts {
                prior: c.rule_prior,
                time: s_time,
                geo: s_geo,
                band: overlap,
                magnitude: magnitude(event),
            };
            let score = parts.prior * parts.time * parts.geo * parts.band * parts.magnitude;
            if score <= 0.0 || score < c.min_score {
                continue;
            }
            let stale = event.valid_until.is_some_and(|v| now > v)
                || feed_states
                    .get(&event.source)
                    .is_some_and(|s| s.is_stale(now));
            out.push(Candidate {
                payload_hash: event.payload_hash()?,
                event: event.clone(),
                score,
                parts,
                time_gap_s: gap_s,
                distance_km: d_km,
                band_overlap: overlap,
                stale,
                cache_age_s: (now.as_unix_nanos() - event.fetched_at.as_unix_nanos()) as f64 / 1e9,
            });
        }
        out.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then(a.event.time.start.cmp(&b.event.time.start))
                .then_with(|| a.event.source.cmp(&b.event.source))
                .then_with(|| a.event.native_id.cmp(&b.event.native_id))
        });
        out.truncate(c.max_explanations);
        Ok(out)
    }

    /// The Explanation a candidate produces (not yet stored).
    pub fn explanation(
        &self,
        anomaly: &Anomaly,
        candidate: &Candidate,
        site: Option<&Site>,
        supersedes: Option<ExplanationId>,
        now: Timestamp,
    ) -> Explanation {
        let value = |name: &str, value: f64| Evidence::Value {
            name: name.to_owned(),
            value,
        };
        let mut evidence = vec![
            Evidence::ExternalEvent {
                id: candidate.event.id,
                payload_hash: candidate.payload_hash,
            },
            Evidence::History {
                region: anomaly.region,
            },
            value("time_gap_s", candidate.time_gap_s),
            value("distance_km", candidate.distance_km),
            value("band_overlap", candidate.band_overlap),
            value("score_time", candidate.parts.time),
            value("score_geo", candidate.parts.geo),
            value("score_band", candidate.parts.band),
            value("score_magnitude", candidate.parts.magnitude),
            value("rule_prior", candidate.parts.prior),
            value("source_stale", if candidate.stale { 1.0 } else { 0.0 }),
            value("cache_age_s", candidate.cache_age_s),
        ];
        if let Some(s) = site {
            evidence.push(value("site_lat_deg", s.lat_deg));
            evidence.push(value("site_lon_deg", s.lon_deg));
        }
        Explanation {
            id: ExplanationId::new(),
            anomaly_ref: anomaly.id,
            cause: Cause::ExternalEvent {
                id: candidate.event.id,
            },
            correlation_type: CorrelationType::TimeCoincidence,
            score: candidate.score,
            evidence,
            supersedes,
            provisional: candidate.stale,
            rule_version: RULE_VERSION.into(),
            t: now,
        }
    }

    /// Writes explanations for ranked `candidates`: idempotent against the latest explanation per
    /// event, superseding changed ones. A candidate pinned to a payload the cache no longer holds
    /// returns `CorrelateError::Repo(RepoError::StaleEvidence)`; rows written before it stay.
    pub fn write(
        &self,
        repo: &mut Repository,
        anomaly: &Anomaly,
        candidates: &[Candidate],
        site: Option<&Site>,
        now: Timestamp,
    ) -> Result<(Vec<Explanation>, Vec<ExplanationId>), CorrelateError> {
        let latest = latest_by_event(repo, anomaly.id)?;
        let (mut written, mut unchanged) = (Vec::new(), Vec::new());
        for candidate in candidates {
            // A candidate ranked before a payload revision must not slip through as "unchanged"
            // (the repository only checks pins on insert).
            let current = repo.external_event(candidate.event.id)?.payload_hash()?;
            if current != candidate.payload_hash {
                return Err(CorrelateError::Repo(RepoError::StaleEvidence {
                    event: candidate.event.id,
                    pinned: candidate.payload_hash,
                    current,
                }));
            }
            let prev = latest.get(&candidate.event.id);
            if let Some(prev) = prev {
                let same_pin =
                    pinned_hash(prev, candidate.event.id) == Some(candidate.payload_hash);
                if same_pin
                    && prev.score.to_bits() == candidate.score.to_bits()
                    && prev.provisional == candidate.stale
                    && prev.rule_version == RULE_VERSION
                {
                    unchanged.push(prev.id);
                    continue;
                }
            }
            let e = self.explanation(anomaly, candidate, site, prev.map(|p| p.id), now);
            repo.insert_explanation(&e)?;
            written.push(e);
        }
        Ok((written, unchanged))
    }

    /// Ranks cached events for an anomaly and writes its explanations (see the module docs).
    /// Reads the feed states from `feeds` first ([`Self::feed_states`]); a caller holding a lock
    /// around `repo` should read them before taking it and use [`Self::correlate_with_states`].
    pub fn correlate(
        &self,
        repo: &mut Repository,
        feeds: Option<&FeedCache>,
        anomaly_id: AnomalyId,
        site: Option<&Site>,
        now: Timestamp,
    ) -> Result<CorrelationOutcome, CorrelateError> {
        let feed_states = self.feed_states(feeds)?;
        self.correlate_with_states(repo, &feed_states, anomaly_id, site, now)
    }

    /// The rule sources' feed states from the cache (file I/O, no repository access).
    pub fn feed_states(
        &self,
        feeds: Option<&FeedCache>,
    ) -> Result<BTreeMap<String, FeedState>, CorrelateError> {
        let mut feed_states = BTreeMap::new();
        if let Some(cache) = feeds {
            for source in &self.config.sources {
                if let Some(state) = cache.state(source)? {
                    feed_states.insert(source.clone(), state);
                }
            }
        }
        Ok(feed_states)
    }

    /// [`Self::correlate`] with feed states read beforehand ([`Self::feed_states`]): touches only
    /// `repo`, so no feed-cache I/O runs while the caller holds a repository lock.
    pub fn correlate_with_states(
        &self,
        repo: &mut Repository,
        feed_states: &BTreeMap<String, FeedState>,
        anomaly_id: AnomalyId,
        site: Option<&Site>,
        now: Timestamp,
    ) -> Result<CorrelationOutcome, CorrelateError> {
        let anomaly = repo.anomaly(anomaly_id)?;
        let mut stale_evidence = Vec::new();
        for attempt in 1..=MAX_ATTEMPTS {
            let window = self.query_window(&anomaly);
            let mut events = Vec::new();
            for source in &self.config.sources {
                events.extend(repo.external_events_overlapping(&window, Some(source))?);
            }
            events.sort_by(|a, b| {
                (a.time.start, &a.source, &a.native_id).cmp(&(
                    b.time.start,
                    &b.source,
                    &b.native_id,
                ))
            });
            for (_, prev) in latest_by_event(repo, anomaly_id)? {
                for ev in &prev.evidence {
                    if let Evidence::ExternalEvent { id, payload_hash } = ev {
                        let current = repo.external_event(*id)?.payload_hash()?;
                        let report = StaleEvidence {
                            explanation: Some(prev.id),
                            event: *id,
                            pinned: *payload_hash,
                            current,
                        };
                        if current != *payload_hash && !stale_evidence.contains(&report) {
                            stale_evidence.push(report);
                        }
                    }
                }
            }
            let candidates = self.rank(&anomaly, site, &events, feed_states, now)?;
            match self.write(repo, &anomaly, &candidates, site, now) {
                Ok((written, unchanged)) => {
                    return Ok(CorrelationOutcome {
                        anomaly: anomaly_id,
                        candidates,
                        written,
                        unchanged,
                        stale_evidence,
                        attempts: attempt,
                    });
                }
                Err(CorrelateError::Repo(RepoError::StaleEvidence {
                    event,
                    pinned,
                    current,
                })) => stale_evidence.push(StaleEvidence {
                    explanation: None,
                    event,
                    pinned,
                    current,
                }),
                Err(e) => return Err(e),
            }
        }
        Err(CorrelateError::Unsettled(MAX_ATTEMPTS))
    }
}

fn pinned_hash(e: &Explanation, event: ExternalEventId) -> Option<ContentHash> {
    e.evidence.iter().find_map(|ev| match ev {
        Evidence::ExternalEvent { id, payload_hash } if *id == event => Some(*payload_hash),
        _ => None,
    })
}

/// Latest (not superseded) explanation per cause event, ordered by event id.
fn latest_by_event(
    repo: &Repository,
    anomaly: AnomalyId,
) -> Result<BTreeMap<ExternalEventId, Explanation>, RepoError> {
    let all = repo.explanations_for_anomaly(anomaly)?;
    let superseded: BTreeSet<ExplanationId> = all.iter().filter_map(|e| e.supersedes).collect();
    let mut latest = BTreeMap::new();
    for e in all {
        if superseded.contains(&e.id) {
            continue;
        }
        if let Cause::ExternalEvent { id } = e.cause {
            latest.entry(id).or_insert(e);
        }
    }
    Ok(latest)
}

#[cfg(test)]
mod tests;
