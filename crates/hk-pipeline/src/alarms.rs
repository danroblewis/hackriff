//! Novelty alarm wiring (T-122, ADR-0012 §7): drives `hk_context::occupancy::alarm`, correlation,
//! and the `anomalies` stream.
//!
//! [`AlarmService`] owns, for one run, the alarm engine (resumed from the run database), the
//! writer (anomaly rows, `anomaly_detail`, explanations through the C30 correlator), the
//! `anomalies` stream (ADR-0004 `messages`, metadata only) and the control API view
//! (`hk_api::anomalies::AnomalyControl`, adapted in hk-cli).
//!
//! **Entry points for the attention loop (T-128).**
//! - [`AlarmService::observe`] takes one scored interval's [`NoveltySnapshot`].
//! - [`AlarmService::observe_fold`] is the thin hook for a T-119 fold: call it with the
//!   `FoldOutcome` that `AttentionService::observe`/`ingest_occupancy` returned, the observation,
//!   its site and calibration keys and the provenance steps near it (scheme 1, 100 kHz cells).
//! - [`AlarmService::set_context`] sets the device site (geometry for correlation) and the feed
//!   states (staleness) read outside the repository lock.
//!
//! All contract time is the sample clock in the snapshots (ADR-0012 §0); a dismissal is stamped
//! with the latest sample time the engine saw (the service clock only before any snapshot).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use hk_context::feeds::FeedState;
use hk_context::geo::Site;
use hk_context::occupancy::alarm::{
    AlarmConfig, AlarmEngine, AlarmEvent, AlarmWriter, DeviceStep, NoveltySnapshot, PoolContext,
    inputs_from_fold, latest_explanations, unscored_evidence,
};
use hk_context::occupancy::baseline::{FoldOutcome, IntervalObservation};
use hk_context::occupancy::novelty::NoveltyConfig;
use hk_model::attention::baseline::{CalKey, HourOfWeek, SiteKey};
use hk_model::repo::alarms::{
    AlarmLifecycle, AlarmRow, AlarmState, AnomalyListing, AnomalyQuery, AnomalyView,
    TOP_EXPLANATIONS, anomaly_view_json,
};
use hk_model::{
    AnomalyId, AnomalyStatus, AnomalyStatusChange, ContentClass, RepoError, Repository, Timestamp,
};
use hk_stream::{MessageRecord, Publisher, PublisherConfig, StreamHeader, StreamKind};
use serde_json::{Value, json};

use crate::attention::{CELL_FACTOR, SCHEME_1_CELL_HZ};
use crate::config::StreamSink;

/// Stream id of the anomalies stream.
pub const ANOMALIES_STREAM_ID: &str = "anomalies";
/// Message schema of the anomalies stream.
pub const ANOMALIES_MESSAGE_SCHEMA: &str = "hackriff.anomaly/1";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn status_of(state: AlarmState) -> AnomalyStatus {
    match state {
        AlarmState::Open => AnomalyStatus::Open,
        AlarmState::Cleared | AlarmState::Explained => AnomalyStatus::Resolved,
        AlarmState::Dismissed => AnomalyStatus::Dismissed,
    }
}

/// A refused or failed control call (hk-cli maps it to `hk_api::anomalies::AnomalyFail`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AlarmFail {
    /// No such anomaly.
    NotFound,
    /// Not applicable to this anomaly.
    Conflict(String),
    /// Storage failure.
    Failed(String),
}

fn repo_fail(e: RepoError) -> AlarmFail {
    match e {
        RepoError::NotFound { .. } => AlarmFail::NotFound,
        other => AlarmFail::Failed(other.to_string()),
    }
}

/// Novelty alarms for one run (see the module docs).
pub struct AlarmService {
    repo: Arc<Mutex<Repository>>,
    engine: Mutex<AlarmEngine>,
    writer: AlarmWriter,
    context: Mutex<(Option<Site>, BTreeMap<String, FeedState>)>,
    publisher: Option<Mutex<Publisher>>,
    clock: Arc<dyn Fn() -> Timestamp + Send + Sync>,
    errors: AtomicU64,
    published: AtomicU64,
    inputs_observed: AtomicU64,
    inputs_mature: AtomicU64,
}

impl AlarmService {
    /// Resumes the engine from the run database and offers the `anomalies` stream through `sink`.
    /// `clock` stamps user dismissals only before the first snapshot.
    pub fn open(
        repo: Arc<Mutex<Repository>>,
        sink: Option<&StreamSink>,
        clock: Arc<dyn Fn() -> Timestamp + Send + Sync>,
    ) -> anyhow::Result<Self> {
        let engine = AlarmEngine::resume(AlarmConfig::default(), &lock(&repo))
            .map_err(|e| anyhow::anyhow!("resuming novelty alarms: {e}"))?;
        let publisher = match sink {
            Some(sink) => {
                let mut header = StreamHeader::new(
                    ANOMALIES_STREAM_ID,
                    StreamKind::Messages,
                    // Anomaly metadata and explanations, never RF content.
                    ContentClass::Unrestricted,
                    format!("hk-pipeline:anomalies@{}", env!("CARGO_PKG_VERSION")),
                );
                header.message_schema = Some(ANOMALIES_MESSAGE_SCHEMA.into());
                header.max_frame_len = 64 * 1024;
                let publisher = Publisher::new(header.clone(), PublisherConfig::default())?;
                sink(&header, publisher.handle());
                Some(Mutex::new(publisher))
            }
            None => None,
        };
        Ok(Self {
            repo,
            engine: Mutex::new(engine),
            writer: AlarmWriter::default(),
            context: Mutex::new((None, BTreeMap::new())),
            publisher,
            clock,
            errors: AtomicU64::new(0),
            published: AtomicU64::new(0),
            inputs_observed: AtomicU64::new(0),
            inputs_mature: AtomicU64::new(0),
        })
    }

    /// Sets the device site (correlation geometry) and the feed states (read by the caller from
    /// `Correlator::feed_states` outside any repository lock).
    pub fn set_context(&self, site: Option<Site>, feed_states: BTreeMap<String, FeedState>) {
        *lock(&self.context) = (site, feed_states);
    }

    /// Steps the engine with one scored interval, writes the transitions and publishes them.
    pub fn observe(&self, snap: &NoveltySnapshot) -> Vec<AlarmEvent> {
        let (site, states) = lock(&self.context).clone();
        let mut engine = lock(&self.engine);
        let actions = engine.step(snap);
        if actions.is_empty() {
            return Vec::new();
        }
        let result = {
            let mut repo = lock(&self.repo);
            self.writer
                .apply(&mut repo, &actions, snap.t, &states, site.as_ref())
        };
        drop(engine);
        match result {
            Ok(events) => {
                for e in &events {
                    self.publish(e);
                }
                events
            }
            Err(_) => {
                self.errors.fetch_add(1, Ordering::Relaxed);
                Vec::new()
            }
        }
    }

    /// T-128 hook: the alarm inputs of one T-119 fold (scheme 1, [`SCHEME_1_CELL_HZ`] level-0
    /// cells, [`CELL_FACTOR`] per baseline cell) observed at `site` under `cal`, with the
    /// provenance `steps` near it.
    #[allow(clippy::too_many_arguments)]
    pub fn observe_fold(
        &self,
        site: SiteKey,
        cal: CalKey,
        obs: &IntervalObservation,
        fold: &FoldOutcome,
        pool: PoolContext,
        utc_offset_min: i16,
        steps: &[DeviceStep],
    ) -> Vec<AlarmEvent> {
        let inputs = self.fold_inputs(site, cal, obs, fold, pool, utc_offset_min);
        if inputs.is_empty() {
            return Vec::new();
        }
        self.observe(&NoveltySnapshot {
            t: obs.t,
            site,
            steps: steps.to_vec(),
            inputs,
        })
    }

    /// T-131: the alarm inputs of one closed occupancy interval ([`crate::attention::IntervalFold`]s
    /// from `AttentionService::ingest_interval`), with the provenance `steps` near it. All folds
    /// of a site go into **one** snapshot, so a gain step is judged against every subject observed
    /// under it (the broadband-shift rule, §7.4) rather than one subject at a time.
    pub fn observe_interval(
        &self,
        folds: &[crate::attention::IntervalFold],
        steps: &[DeviceStep],
    ) -> Vec<AlarmEvent> {
        let mut by_site: BTreeMap<SiteKey, (Timestamp, Vec<_>)> = BTreeMap::new();
        for f in folds {
            let inputs = self.fold_inputs(f.site, f.cal, &f.obs, &f.fold, f.pool, f.utc_offset_min);
            let e = by_site.entry(f.site).or_insert((f.obs.t, Vec::new()));
            e.0 = e.0.min(f.obs.t);
            e.1.extend(inputs);
        }
        let mut events = Vec::new();
        for (site, (t, inputs)) in by_site {
            if inputs.is_empty() {
                continue;
            }
            events.extend(self.observe(&NoveltySnapshot {
                t,
                site,
                steps: steps.to_vec(),
                inputs,
            }));
        }
        events
    }

    /// The scored inputs of one fold; its unscored evidence (immature pool, mobile/unassigned
    /// site) is counted as suppressions here, never raised (§7.3). Bumps the input counters.
    fn fold_inputs(
        &self,
        site: SiteKey,
        cal: CalKey,
        obs: &IntervalObservation,
        fold: &FoldOutcome,
        pool: PoolContext,
        utc_offset_min: i16,
    ) -> Vec<hk_context::occupancy::alarm::AlarmInput> {
        let inputs = inputs_from_fold(
            obs,
            fold,
            cal,
            HourOfWeek::of(obs.t, utc_offset_min),
            1,
            SCHEME_1_CELL_HZ,
            i64::from(CELL_FACTOR),
            pool,
            &NoveltyConfig::default(),
        );
        let unscored = unscored_evidence(obs, fold);
        if !unscored.is_empty() {
            lock(&self.engine).count_unscored(
                obs.t,
                site,
                fold.novelty.maturity,
                fold.novelty.provenance_explained,
                &unscored,
            );
        }
        let mature = inputs.iter().filter(|i| i.maturity.is_mature()).count();
        self.inputs_observed
            .fetch_add((inputs.len() + unscored.len()) as u64, Ordering::Relaxed);
        self.inputs_mature
            .fetch_add(mature as u64, Ordering::Relaxed);
        inputs
    }

    /// Counters and suppressions (for `/api/status` and the report). `inputs_observed` counts
    /// every per-kind piece of fold evidence (scored or not), `inputs_mature` the scored inputs
    /// from mature pools: zero observed means nothing reached the alarms, observed without mature
    /// means everything was filtered (see `suppressions`).
    pub fn status_json(&self) -> Value {
        json!({
            "open": lock(&self.engine).open_alarms().len(),
            "inputs_observed": self.inputs_observed.load(Ordering::Relaxed),
            "inputs_mature": self.inputs_mature.load(Ordering::Relaxed),
            "errors": self.errors.load(Ordering::Relaxed),
            "published": self.published.load(Ordering::Relaxed),
            "suppressions": self.suppressions(),
        })
    }

    fn publish(&self, e: &AlarmEvent) {
        let Some(p) = &self.publisher else { return };
        let view = AnomalyView {
            listing: AnomalyListing {
                anomaly: e.anomaly.clone(),
                status: status_of(e.row.state),
                alarm: Some(e.row.clone()),
            },
            explanations: e.explanations.clone(),
            history: Vec::new(),
        };
        let msg = MessageRecord {
            t: e.row.last_t,
            emitter_id: None,
            provenance_ref: None,
            content_class: ContentClass::Unrestricted,
            decode_id: None,
            annotation_id: None,
            decoder: None,
            frame_model: Some("anomaly".into()),
            crc_status: None,
            identity: None,
            metadata: json!({
                "kind": "anomaly",
                "transition": e.transition,
                "anomaly": anomaly_view_json(&view, Some(TOP_EXPLANATIONS)),
            }),
            content: None,
        };
        // A slow subscriber loses messages (the publisher's drop policy), never the rows.
        if lock(p).publish_message(&msg).is_ok() {
            self.published.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn view(&self, repo: &Repository, id: AnomalyId) -> Result<AnomalyView, AlarmFail> {
        let anomaly = repo.anomaly(id).map_err(repo_fail)?;
        Ok(AnomalyView {
            listing: AnomalyListing {
                anomaly,
                status: repo.anomaly_current_status(id).map_err(repo_fail)?,
                alarm: repo.alarm_row(id).map_err(repo_fail)?,
            },
            explanations: latest_explanations(repo, id).map_err(repo_fail)?,
            history: repo.anomaly_status_history(id).map_err(repo_fail)?,
        })
    }

    fn transition(
        &self,
        id: AnomalyId,
        change: impl FnOnce(
            &mut AlarmEngine,
            &mut AlarmRow,
            Timestamp,
        ) -> Result<AnomalyStatusChange, AlarmFail>,
        lifecycle: AlarmLifecycle,
    ) -> Result<AnomalyView, AlarmFail> {
        let mut engine = lock(&self.engine);
        let t = engine.now().unwrap_or_else(|| (self.clock)());
        let (view, event) = {
            let mut repo = lock(&self.repo);
            let mut row = repo.alarm_row(id).map_err(repo_fail)?.ok_or_else(|| {
                if repo.anomaly(id).is_ok() {
                    AlarmFail::Conflict("only novelty alarms can be dismissed or reopened".into())
                } else {
                    AlarmFail::NotFound
                }
            })?;
            let status = change(&mut engine, &mut row, t)?;
            row.last_transition = lifecycle;
            row.last_t = row.last_t.max(t);
            repo.update_alarm(&row, Some(&status)).map_err(repo_fail)?;
            let view = self.view(&repo, id)?;
            let event = AlarmEvent {
                transition: lifecycle,
                anomaly: view.listing.anomaly.clone(),
                row,
                explanations: view.explanations.clone(),
            };
            (view, event)
        };
        drop(engine);
        self.publish(&event);
        Ok(view)
    }
}

/// Control API view (`/api/anomalies*`, adapted in hk-cli).
impl AlarmService {
    /// A page of anomalies with their top explanations, and the next cursor.
    pub fn list(&self, q: &AnomalyQuery) -> Result<(Vec<AnomalyView>, Option<usize>), AlarmFail> {
        let repo = lock(&self.repo);
        let page = repo.anomalies_page(q).map_err(repo_fail)?;
        let rows = page
            .rows
            .into_iter()
            .map(|listing| {
                let explanations =
                    latest_explanations(&repo, listing.anomaly.id).map_err(repo_fail)?;
                Ok(AnomalyView {
                    listing,
                    explanations,
                    history: Vec::new(),
                })
            })
            .collect::<Result<Vec<_>, AlarmFail>>()?;
        Ok((rows, page.next_cursor))
    }

    /// One anomaly with all current explanations and its history.
    pub fn get(&self, id: AnomalyId) -> Result<AnomalyView, AlarmFail> {
        self.view(&lock(&self.repo), id)
    }

    /// Dismisses a novelty alarm (7 days of sample time).
    pub fn dismiss(&self, id: AnomalyId, note: Option<String>) -> Result<AnomalyView, AlarmFail> {
        self.transition(
            id,
            |engine, row, t| {
                if row.state == AlarmState::Explained {
                    return Err(AlarmFail::Conflict(
                        "a self-inflicted anomaly is not a novelty alarm".into(),
                    ));
                }
                let until = engine.dismiss(row.key, t);
                row.state = AlarmState::Dismissed;
                row.dismissed_until = Some(until);
                Ok(AnomalyStatusChange {
                    anomaly_id: id,
                    status: AnomalyStatus::Dismissed,
                    t,
                    note: Some(match note {
                        Some(n) => format!("dismissed;until_ns={};note={n}", until.as_unix_nanos()),
                        None => format!("dismissed;until_ns={}", until.as_unix_nanos()),
                    }),
                })
            },
            AlarmLifecycle::Dismissed,
        )
    }

    /// Lifts a dismissal (or re-opens a cleared alarm).
    pub fn reopen(&self, id: AnomalyId) -> Result<AnomalyView, AlarmFail> {
        self.transition(
            id,
            |engine, row, t| {
                if !matches!(row.state, AlarmState::Dismissed | AlarmState::Cleared) {
                    return Err(AlarmFail::Conflict(
                        "only a dismissed or cleared alarm can be reopened".into(),
                    ));
                }
                engine.reopen(row.key, id);
                row.state = AlarmState::Open;
                row.dismissed_until = None;
                row.reopen_count += 1;
                Ok(AnomalyStatusChange {
                    anomaly_id: id,
                    status: AnomalyStatus::Open,
                    t,
                    note: Some("reopened-by-user".into()),
                })
            },
            AlarmLifecycle::Reopened,
        )
    }

    /// Suppression counts per alarm kind (ADR-0012 §7.3).
    pub fn suppressions(&self) -> Value {
        let engine = lock(&self.engine);
        let mut by_kind: BTreeMap<&'static str, BTreeMap<&'static str, u64>> = BTreeMap::new();
        for ((kind, name), n) in &engine.suppressions().counts {
            *by_kind
                .entry(kind.as_str())
                .or_default()
                .entry(name.0)
                .or_default() += n;
        }
        json!(by_kind)
    }
}

#[cfg(test)]
mod tests {
    use hk_context::occupancy::alarm::AlarmInput;
    use hk_model::FreqRange;
    use hk_model::attention::alarm::{AlarmKind, AlarmSubject, AlarmUnit};
    use hk_model::attention::baseline::{BaselineResolution, Maturity};
    use hk_model::ids::SiteId;

    use super::*;

    fn at(i: i64) -> Timestamp {
        Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + i * 900_000_000_000)
    }

    fn snap(site: SiteKey, i: i64, novelty: f64) -> NoveltySnapshot {
        NoveltySnapshot {
            t: at(i),
            site,
            steps: Vec::new(),
            inputs: vec![AlarmInput {
                kind: AlarmKind::BusierThanUsual,
                subject: AlarmSubject::Cells {
                    scheme: 1,
                    lo_cell: 69_280,
                    hi_cell: 69_296,
                },
                freq: FreqRange::new(433.0e6, 433.1e6),
                cal: CalKey::Uncalibrated,
                resolution: BaselineResolution::AllHours,
                slot: HourOfWeek::of(at(i), 0),
                maturity: Maturity::Mature {
                    resolution: BaselineResolution::AllHours,
                },
                provenance_explained: false,
                unit: AlarmUnit::Fraction,
                observed: 0.7,
                baseline_mean: 0.01,
                baseline_spread: 0.02,
                z: 30.0,
                novelty,
                observed_s: 900.0,
            }],
        }
    }

    /// Raise → list/get → dismiss (suppressed while dismissed) → reopen, and a resumed service
    /// keeps the state.
    #[test]
    fn alarm_service_raises_lists_dismisses_and_reopens() {
        let repo = Arc::new(Mutex::new(Repository::open_in_memory().unwrap()));
        let clock: Arc<dyn Fn() -> Timestamp + Send + Sync> = Arc::new(|| at(0));
        let svc = AlarmService::open(Arc::clone(&repo), None, Arc::clone(&clock)).unwrap();
        let site = SiteKey::Site(SiteId::new());
        assert!(svc.observe(&snap(site, 0, 0.9)).is_empty());
        let raised = svc.observe(&snap(site, 1, 0.9));
        assert_eq!(raised.len(), 1);
        let id = raised[0].anomaly.id;
        let (rows, next) = svc
            .list(&AnomalyQuery {
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!((rows.len(), next), (1, None));
        assert_eq!(rows[0].listing.status, AnomalyStatus::Open);
        assert!(!rows[0].explanations.is_empty());

        let v = svc.dismiss(id, Some("known neighbour".into())).unwrap();
        assert_eq!(v.listing.status, AnomalyStatus::Dismissed);
        assert!(svc.observe(&snap(site, 2, 0.9)).is_empty());
        assert!(svc.observe(&snap(site, 3, 0.9)).is_empty());
        assert_eq!(svc.suppressions()["busier-than-usual"]["dismissed"], 2);

        let resumed = AlarmService::open(Arc::clone(&repo), None, clock).unwrap();
        assert!(
            resumed.observe(&snap(site, 4, 0.9)).is_empty(),
            "still dismissed"
        );
        let v = resumed.reopen(id).unwrap();
        assert_eq!(v.listing.status, AnomalyStatus::Open);
        assert_eq!(
            v.history.last().unwrap().note.as_deref(),
            Some("reopened-by-user")
        );
        assert!(matches!(resumed.reopen(id), Err(AlarmFail::Conflict(_))));
        assert!(matches!(
            resumed.get(AnomalyId::new()),
            Err(AlarmFail::NotFound)
        ));
    }

    /// T-131 review: immature folds with evidence on the live path (`observe_interval`) raise
    /// nothing and are counted as `immature-baseline` suppressions, and the input counters tell
    /// observed evidence from mature inputs.
    #[test]
    fn alarm_service_counts_immature_folds_as_suppressions() {
        use crate::attention::IntervalFold;
        use hk_context::occupancy::alarm::PoolContext;
        use hk_model::attention::occupancy::{ChannelKey, OccupancySubject};
        use hk_model::attention::score::NoveltyScore;
        use hk_store::baseline::BaselineSubject;

        let repo = Arc::new(Mutex::new(Repository::open_in_memory().unwrap()));
        let svc =
            AlarmService::open(repo, None, Arc::new(|| Timestamp::from_unix_nanos(0))).unwrap();
        let site = SiteKey::Site(SiteId::new());
        let key = ChannelKey {
            scheme: 1,
            lo_cell: 69_000,
            hi_cell: 69_004,
        };
        let fold = |i: i64| IntervalFold {
            subject: OccupancySubject::Channel { key },
            site,
            cal: CalKey::Uncalibrated,
            obs: IntervalObservation {
                subject: BaselineSubject::Channel { key },
                t: at(i),
                gain: 0,
                level_db: Some(-40.0),
                max_db: Some(-30.0),
                occupied_weight_s: 900.0,
                weight_s: 900.0,
                observed_s: 900.0,
                n_eff: 12.0,
                suspect_fraction: 0.0,
                provenance_explained: false,
            },
            utc_offset_min: 0,
            pool: PoolContext::default(),
            fold: FoldOutcome {
                novelty: NoveltyScore {
                    novelty: 0.0,
                    level_z: None,
                    occupancy_z: None,
                    new_emitter: None,
                    observed_s: 900.0,
                    maturity: Maturity::Immature {
                        observed_s: 900.0 * i as f64,
                    },
                    provenance_explained: false,
                },
                change_point: None,
                accrued: true,
                accrued_reference: true,
            },
        };
        for i in 0..10 {
            let events = svc.observe_interval(&[fold(i)], &[]);
            assert!(events.is_empty(), "interval {i}: {events:?}");
        }
        let s = svc.suppressions();
        assert_eq!(s["busier-than-usual"]["immature-baseline"], 10, "{s}");
        assert_eq!(s["level-above-baseline"]["immature-baseline"], 10, "{s}");
        let status = svc.status_json();
        assert_eq!(status["inputs_observed"], 20, "{status}");
        assert_eq!(status["inputs_mature"], 0, "{status}");
        assert_eq!(status["open"], 0);
    }
}
