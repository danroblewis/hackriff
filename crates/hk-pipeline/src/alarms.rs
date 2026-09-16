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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use hk_context::feeds::FeedState;
use hk_context::geo::Site;
use hk_context::occupancy::alarm::{
    AlarmConfig, AlarmEngine, AlarmEvent, AlarmInput, AlarmWriter, DeviceStep, NoveltySnapshot,
    PoolContext, inputs_from_fold, latest_explanations, subject_of, unscored_evidence,
};
use hk_context::occupancy::baseline::{FoldOutcome, IntervalObservation};
use hk_context::occupancy::novelty::NoveltyConfig;
use hk_context::watch::{self, StandingRelation, WatchActivity, WatchDecision, WatchRegion};
use hk_model::attention::baseline::{CalKey, HourOfWeek, SiteKey};
use hk_model::repo::alarms::{
    AlarmLifecycle, AlarmRow, AlarmState, AnomalyListing, AnomalyQuery, AnomalyView,
    TOP_EXPLANATIONS, anomaly_view_json,
};
use hk_model::{
    Anomaly, AnomalyId, AnomalyKind, AnomalyStatus, AnomalyStatusChange, AnomalySubject, Cause,
    ContentClass, CorrelationType, EmitterId, Evidence, Explanation, ExplanationId, FreqRange,
    Region, RepoError, Repository, SelectionId, TimeRange, Timestamp,
};
use hk_stream::{MessageRecord, Publisher, PublisherConfig, StreamHeader, StreamKind};
use serde_json::{Value, json};

use crate::attention::{CELL_FACTOR, FirstSighting, SCHEME_1_CELL_HZ};
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
    /// T-166: per-selection region-watch state (what each watch has already told the user, and
    /// what it decided not to). Not persisted, like the hysteresis' building counts.
    watch: Mutex<WatchState>,
    watch_alerts: AtomicU64,
    watch_suppressed: AtomicU64,
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
            watch: Mutex::new(WatchState::default()),
            watch_alerts: AtomicU64::new(0),
            watch_suppressed: AtomicU64::new(0),
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
        self.observe_interval_with_new_emitters(
            folds,
            SiteKey::Unassigned,
            Timestamp::UNIX_EPOCH,
            &[],
            steps,
        )
    }

    /// T-136: [`Self::observe_interval`] plus the close's new-emitter inputs
    /// (`AttentionService::new_emitter_inputs`) observed at `site` at `t`. The mature ones join that
    /// site's fold snapshot, so a gain step is judged across both (§7.4); immature ones are
    /// counted as suppressions, never raised (§7.3). Each sighting is counted (observed, mature,
    /// suppressed) once, at the close where it is `fresh`; later closes re-feed it uncounted.
    pub fn observe_interval_with_new_emitters(
        &self,
        folds: &[crate::attention::IntervalFold],
        site: SiteKey,
        t: Timestamp,
        new_emitters: &[crate::attention::NewEmitterInput],
        steps: &[DeviceStep],
    ) -> Vec<AlarmEvent> {
        let mut by_site: BTreeMap<SiteKey, (Timestamp, Vec<_>)> = BTreeMap::new();
        for f in folds {
            let inputs = self.fold_inputs(f.site, f.cal, &f.obs, &f.fold, f.pool, f.utc_offset_min);
            let e = by_site.entry(f.site).or_insert((f.obs.t, Vec::new()));
            e.0 = e.0.min(f.obs.t);
            e.1.extend(inputs);
        }
        if !new_emitters.is_empty() {
            type Ne<'a> = Vec<&'a crate::attention::NewEmitterInput>;
            let (mature, immature): (Ne<'_>, Ne<'_>) = new_emitters
                .iter()
                .partition(|n| n.input.maturity.is_mature());
            let fresh_mature = mature.iter().filter(|n| n.fresh).count() as u64;
            let immature: Vec<AlarmInput> = immature
                .into_iter()
                .filter(|n| n.fresh)
                .map(|n| n.input)
                .collect();
            let mature: Vec<AlarmInput> = mature.into_iter().map(|n| n.input).collect();
            self.inputs_observed
                .fetch_add(fresh_mature + immature.len() as u64, Ordering::Relaxed);
            self.inputs_mature
                .fetch_add(fresh_mature, Ordering::Relaxed);
            // An immature rate scores nothing (novelty 0): the engine would drop such cold cells
            // before counting them, so they are counted here, one per new sighting, never raised.
            if !immature.is_empty() {
                let mut engine = lock(&self.engine);
                for i in &immature {
                    engine.count_unscored(
                        t,
                        site,
                        i.maturity,
                        false,
                        &[hk_model::attention::alarm::AlarmKind::NewEmitter],
                    );
                }
            }
            if !mature.is_empty() {
                let e = by_site.entry(site).or_insert((t, Vec::new()));
                e.0 = e.0.min(t);
                e.1.extend(mature);
            }
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
            let mut engine = lock(&self.engine);
            engine.count_unscored(
                obs.t,
                site,
                fold.novelty.maturity,
                fold.novelty.provenance_explained,
                &unscored,
            );
            // T-146: an unscorable (immature) fold ends the subject's busier/quieter run.
            let (subject, _) =
                subject_of(&obs.subject, 1, SCHEME_1_CELL_HZ, i64::from(CELL_FACTOR));
            engine.reset_sequential(site, &subject);
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
            // T-166: region-watch alerts raised, and sightings the watches decided not to alert
            // on (mostly rows the T-219 rules explain as another row). Disclosed, never silent.
            "watch_alerts": self.watch_alerts.load(Ordering::Relaxed),
            "watch_suppressed": self.watch_suppressed.load(Ordering::Relaxed),
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

// ---------------------------------------------------------------------------------------------
// T-166: region watch (ADR-0013 §4.9 gap 9)
// ---------------------------------------------------------------------------------------------

/// Most alerts kept per selection for its watch report.
pub const WATCH_ALERTS_KEPT: usize = 256;

/// Most skipped sightings kept per selection for its watch report.
pub const WATCH_SKIPS_KEPT: usize = 64;

/// One alert a region watch raised.
#[derive(Clone, Debug, PartialEq)]
pub struct WatchAlertRecord {
    /// The watched selection.
    pub selection: SelectionId,
    /// The anomaly row written for it.
    pub anomaly: AnomalyId,
    /// The emission.
    pub emitter: EmitterId,
    /// Its measured extent.
    pub freq: FreqRange,
    /// When (sample clock).
    pub t: Timestamp,
    /// Why it counted as new activity.
    pub reason: String,
}

/// One sighting a region watch decided not to alert on, with its reasoning.
#[derive(Clone, Debug, PartialEq)]
pub struct WatchSkipRecord {
    /// The watched selection.
    pub selection: SelectionId,
    /// The emission.
    pub emitter: EmitterId,
    /// Why not.
    pub reason: watch::WatchSkipReason,
    /// The T-219 relationship that stopped it, when one did.
    pub relation: Option<StandingRelation>,
    /// When (sample clock).
    pub t: Timestamp,
    /// The reasoning, rendered.
    pub text: String,
}

/// One region-watch alert, as [`AlarmService::observe_watches`] returns it.
#[derive(Clone, Debug, PartialEq)]
pub struct WatchEvent {
    /// The alert.
    pub alert: WatchAlertRecord,
    /// The anomaly row.
    pub anomaly: Anomaly,
    /// Its explanation, carrying the reasoning.
    pub explanation: Explanation,
}

/// What one selection's watch has done, for `GET /api/selections/{id}/watch`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WatchReport {
    /// Whether a watch is armed right now.
    pub armed: bool,
    /// Alerts raised, newest last.
    pub alerts: Vec<WatchAlertRecord>,
    /// Sightings not alerted on, newest last (bounded by [`WATCH_SKIPS_KEPT`]).
    pub skipped: Vec<WatchSkipRecord>,
    /// Alerts raised since the service started.
    pub alerted_total: u64,
    /// Sightings suppressed since the service started.
    pub suppressed_total: u64,
}

#[derive(Default)]
struct WatchState {
    /// Emitters each watch has already alerted on (one alert per emitter per watch).
    alerted: BTreeMap<SelectionId, BTreeSet<EmitterId>>,
    alerts: BTreeMap<SelectionId, VecDeque<WatchAlertRecord>>,
    skipped: BTreeMap<SelectionId, VecDeque<WatchSkipRecord>>,
    totals: BTreeMap<SelectionId, (u64, u64)>,
}

fn push_bounded<T>(q: &mut VecDeque<T>, item: T, cap: usize) {
    q.push_back(item);
    while q.len() > cap {
        q.pop_front();
    }
}

impl AlarmService {
    /// T-166: runs the armed region watches over one close's inventory first sightings.
    ///
    /// This is the only place the watch rule meets the repository, and the order matters. For
    /// each sighting it reads the **relationships currently in force** on that row
    /// (`Repository::emitter_relations`, the T-219 claims) and hands them to
    /// [`hk_context::watch::evaluate`], which refuses to call a row new activity when it is
    /// suppressed by a Confirmed entry, a duplicate of a stronger candidate, or attributed as an
    /// image / harmonic / intermod of a confirmed source.
    ///
    /// **A relationship read that fails suppresses the alert rather than raising it.** The unsafe
    /// direction is alerting on an image because the lookup was unavailable, which is exactly when
    /// the user would stop trusting the watch.
    ///
    /// Alerts are anomalies plus explanations plus stream messages. Nothing here tunes, records,
    /// deletes or changes any other row.
    pub fn observe_watches(&self, sightings: &[FirstSighting], t: Timestamp) -> Vec<WatchEvent> {
        if sightings.is_empty() {
            return Vec::new();
        }
        let mut repo = lock(&self.repo);
        let selections = match repo.selections() {
            Ok(s) => s,
            Err(_) => {
                self.errors.fetch_add(1, Ordering::Relaxed);
                return Vec::new();
            }
        };
        let regions = watch::armed_regions(&selections);
        if regions.is_empty() {
            return Vec::new();
        }
        let mut state = lock(&self.watch);
        let mut out = Vec::new();
        for s in sightings {
            let standing = match repo.emitter_relations(s.emitter) {
                Ok(r) => StandingRelation::standing(&r),
                Err(_) => {
                    // Fail closed: without the relationships we cannot tell new activity from the
                    // receiver's own image of something already confirmed.
                    self.errors.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };
            let activity = WatchActivity {
                emitter: s.emitter,
                freq: s.freq,
                t: s.t,
                relations: standing,
            };
            for region in &regions {
                let empty = BTreeSet::new();
                let already = state.alerted.get(&region.selection).unwrap_or(&empty);
                match watch::evaluate(region, &activity, already) {
                    WatchDecision::Alert(reason) => {
                        match self.raise_watch_alert(&mut repo, region, &activity, reason, t) {
                            Ok(event) => {
                                state
                                    .alerted
                                    .entry(region.selection)
                                    .or_default()
                                    .insert(activity.emitter);
                                push_bounded(
                                    state.alerts.entry(region.selection).or_default(),
                                    event.alert.clone(),
                                    WATCH_ALERTS_KEPT,
                                );
                                state.totals.entry(region.selection).or_default().0 += 1;
                                self.watch_alerts.fetch_add(1, Ordering::Relaxed);
                                self.publish_watch(&event, region);
                                out.push(event);
                            }
                            Err(_) => {
                                self.errors.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    WatchDecision::Skip(skip) => {
                        // Outside the extent is not this watch's business and is not counted as a
                        // suppression; the other reasons are, and are kept with their reasoning.
                        if skip.reason == watch::WatchSkipReason::OutsideExtent {
                            continue;
                        }
                        push_bounded(
                            state.skipped.entry(region.selection).or_default(),
                            WatchSkipRecord {
                                selection: region.selection,
                                emitter: activity.emitter,
                                reason: skip.reason,
                                relation: skip.relation.clone(),
                                t: activity.t,
                                text: skip.text.clone(),
                            },
                            WATCH_SKIPS_KEPT,
                        );
                        state.totals.entry(region.selection).or_default().1 += 1;
                        self.watch_suppressed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        out
    }

    /// Writes one alert: an anomaly row plus the explanation that carries the rule's reasoning.
    ///
    /// The anomaly's `score` is 1 and its `baseline_ref` is a `region-watch:v1` key, never a
    /// `c12-alarm:v1` one: a watch alert has **no baseline** and claims no measured novelty. It
    /// says "this is new inside the extent you asked to watch", which is why the user is told.
    fn raise_watch_alert(
        &self,
        repo: &mut Repository,
        region: &WatchRegion,
        activity: &WatchActivity,
        reason: String,
        t: Timestamp,
    ) -> Result<WatchEvent, RepoError> {
        let anomaly = Anomaly {
            id: AnomalyId::new(),
            kind: AnomalyKind::NewEmitter,
            subject: AnomalySubject::Emitter(activity.emitter),
            region: Region::new(activity.freq, TimeRange::new(activity.t, activity.t)),
            score: 1.0,
            baseline_ref: Some(watch::baseline_ref(region.selection, activity.emitter)),
            t,
            detector_version: watch::RULE_VERSION.into(),
        };
        repo.insert_anomaly(&anomaly)?;
        let explanation = Explanation {
            id: ExplanationId::new(),
            anomaly_ref: anomaly.id,
            // The device's own memory of what the user asked to watch is what raised this, and
            // the description is the rule's reasoning in words. Always disclosed.
            cause: Cause::OwnHistory {
                description: reason.clone(),
            },
            correlation_type: CorrelationType::Geometry,
            score: 1.0,
            evidence: vec![
                Evidence::Value {
                    name: "f_lo_hz".into(),
                    value: activity.freq.lo_hz,
                },
                Evidence::Value {
                    name: "f_hi_hz".into(),
                    value: activity.freq.hi_hz,
                },
            ],
            supersedes: None,
            provisional: false,
            rule_version: watch::RULE_VERSION.into(),
            t,
        };
        repo.insert_explanation(&explanation)?;
        Ok(WatchEvent {
            alert: WatchAlertRecord {
                selection: region.selection,
                anomaly: anomaly.id,
                emitter: activity.emitter,
                freq: activity.freq,
                t: activity.t,
                reason,
            },
            anomaly,
            explanation,
        })
    }

    /// Publishes a watch alert on the `anomalies` stream, in the documented list-row shape plus a
    /// `watch` block naming the selection whose watch fired.
    fn publish_watch(&self, e: &WatchEvent, region: &WatchRegion) {
        let Some(p) = &self.publisher else { return };
        let view = AnomalyView {
            listing: AnomalyListing {
                anomaly: e.anomaly.clone(),
                status: AnomalyStatus::Open,
                // No AlarmRow: a watch alert is not a baseline novelty alarm.
                alarm: None,
            },
            explanations: vec![e.explanation.clone()],
            history: Vec::new(),
        };
        let msg = MessageRecord {
            t: e.alert.t,
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
                "transition": AlarmLifecycle::Raised,
                "anomaly": anomaly_view_json(&view, Some(TOP_EXPLANATIONS)),
                "watch": {
                    "selection_id": region.selection.to_string(),
                    "name": region.name,
                    "f_lo": region.freq.lo_hz,
                    "f_hi": region.freq.hi_hz,
                    "reason": e.alert.reason,
                },
            }),
            content: None,
        };
        if lock(p).publish_message(&msg).is_ok() {
            self.published.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// T-166: what one selection's watch has done. `NotFound` when there is no such selection.
    pub fn watch_report(&self, id: SelectionId) -> Result<WatchReport, AlarmFail> {
        let armed = {
            let repo = lock(&self.repo);
            match repo.selection(id) {
                Ok(s) => s.watching(),
                Err(RepoError::NotFound { .. }) => return Err(AlarmFail::NotFound),
                Err(e) => return Err(AlarmFail::Failed(e.to_string())),
            }
        };
        let state = lock(&self.watch);
        let (alerted_total, suppressed_total) = state.totals.get(&id).copied().unwrap_or((0, 0));
        Ok(WatchReport {
            armed,
            alerts: state
                .alerts
                .get(&id)
                .map(|q| q.iter().cloned().collect())
                .unwrap_or_default(),
            skipped: state
                .skipped
                .get(&id)
                .map(|q| q.iter().cloned().collect())
                .unwrap_or_default(),
            alerted_total,
            suppressed_total,
        })
    }
}

#[cfg(test)]
mod watch_tests {
    use hk_model::*;

    use super::*;

    fn t(sec: i64) -> Timestamp {
        Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
    }

    fn tr(a: i64, b: i64) -> TimeRange {
        TimeRange::new(t(a), t(b))
    }

    /// A repository with one survey to hang detections off.
    fn scene() -> (Repository, SurveyId) {
        let mut r = Repository::open_in_memory().unwrap();
        let plan = ScanPlan {
            id: ScanPlanId::new(),
            version: 1,
            name: "fm".into(),
            created_at: t(0),
            regions: vec![PlanRegion {
                freq: FreqRange::new(88e6, 108e6),
                priority: 1.0,
                revisit_ns: None,
            }],
            policy: ScanPolicy::SweepThenDwell,
            gain_table: vec![],
            schedule: Schedule::Cron {
                expr: "* * * * *".into(),
            },
            extra: serde_json::Value::Null,
        };
        r.insert_scan_plan(&plan).unwrap();
        let survey = Survey {
            id: SurveyId::new(),
            plan_id: plan.id,
            plan_version: plan.version,
            device_id: "test".into(),
            state: SurveyState::Open,
            t_start: t(0),
            t_end: None,
            summary: None,
        };
        r.insert_survey(&survey).unwrap();
        let id = survey.id;
        (r, id)
    }

    fn prov(r: &mut Repository, lo: f64) -> ProvenanceId {
        r.intern_provenance(&Provenance {
            device_id: "test".into(),
            tune: Tune {
                center_hz: lo,
                sample_rate_hz: 2.4e6,
                lna_db: 32.0,
                vga_db: 30.0,
                amp_on: true,
                bandwidth_hz: 1.75e6,
            },
            overload: false,
            quantisation_limited: false,
            temperature_c: None,
            antenna_port: None,
            clock_source: ClockSource::Internal,
            clock_locked: true,
            calibration_state_ref: None,
            spur_mask_ref: None,
            timestamp_method: TimestampMethod::Synthetic,
            timestamp_error_budget_ns: None,
        })
        .unwrap()
    }

    /// One inventory row from a track sighting, with a detection linked through its track so the
    /// T-219 rules can read its level, −3 dB width, suspect flags and tuning centre.
    #[allow(clippy::too_many_arguments)]
    fn station(
        r: &mut Repository,
        survey: SurveyId,
        provenance: ProvenanceId,
        f: f64,
        obw: f64,
        snr: f64,
        peak_dbfs: f32,
        seen: TimeRange,
        flags: DetectionFlags,
    ) -> EmitterId {
        let track_id = TrackId::new();
        let fp = Fingerprint {
            duty_cycle: Some(1.0),
            ..Fingerprint::new(f, obw)
        };
        let id = r
            .record_sighting(
                &Sighting {
                    source: LinkTarget::Track(track_id),
                    seen,
                    count: 4,
                    f_center_hz: f,
                    bandwidth_hz: obw,
                    fingerprint: Some(fp),
                    identity: None,
                    context: None,
                    classification: None,
                    tags: Vec::new(),
                },
                None,
            )
            .unwrap()
            .emitter_id;
        let d = Detection {
            id: DetectionId::new(),
            survey_id: survey,
            time: seen,
            f_center_hz: f,
            obw_hz: obw,
            xdb_bandwidth_hz: Some(obw),
            xdb_level_db: Some(-3.0),
            snr_peak_db: snr,
            snr_mean_db: snr - 3.0,
            peak_level_dbfs: peak_dbfs,
            peak_level_dbm: None,
            sk: None,
            clip_count: 0,
            detector_version: "test@1".into(),
            provenance_ref: provenance,
            flags,
        };
        r.insert_detections(std::slice::from_ref(&d)).unwrap();
        r.upsert_track(&Track {
            id: track_id,
            state: TrackState::Closed,
            split_from: None,
            time: seen,
            f_center_hz: f,
            bandwidth_hz: obw,
            detection_count: 1,
            timing: TimingFeatures {
                duty_cycle: Some(1.0),
                ..TimingFeatures::default()
            },
            updated_at: seen.end,
        })
        .unwrap();
        r.link_detections_to_track(track_id, &[d.id], seen.end)
            .unwrap();
        id
    }

    fn confirm(r: &mut Repository, id: EmitterId) {
        r.change_emitter_lifecycle(
            id,
            LifecycleState::Confirmed,
            LifecycleAuthor::Auto,
            "test/confirm@1",
            "steady trusted track",
            t(5),
        )
        .unwrap();
    }

    fn service(repo: Repository) -> AlarmService {
        AlarmService::open(
            Arc::new(Mutex::new(repo)),
            None,
            Arc::new(|| Timestamp::from_unix_nanos(0)),
        )
        .unwrap()
    }

    fn sighting(emitter: EmitterId, f: f64, bw: f64) -> FirstSighting {
        FirstSighting {
            emitter,
            freq: FreqRange::centered(f, bw),
            t: t(10),
        }
    }

    /// **The T-166 × T-219 interaction, end to end.**
    ///
    /// The relationships here are *derived from the measurements* by `resolve_overlaps` — the real
    /// T-219 rules, not hand-written claims — and they are read back from the repository by the
    /// watch service itself. So this also proves the relation data is reachable from where the
    /// watch rule runs.
    ///
    /// Inside one watched selection: a genuinely new emission, the receiver's image of a confirmed
    /// station, and a second box over that same confirmed station. Only the first is new activity.
    /// A watch that fired on the other two would be teaching the user to ignore it.
    #[test]
    fn a_region_watch_alerts_on_new_activity_and_never_on_a_suppressed_or_attributed_row() {
        let (mut r, sv) = scene();
        let p = prov(&mut r, 100.8e6);
        let tol = Tolerances::default();

        // A strong confirmed station at 101.3 MHz.
        let source = station(
            &mut r,
            sv,
            p,
            101.3e6,
            180e3,
            26.0,
            -18.0,
            tr(0, 5),
            DetectionFlags::default(),
        );
        confirm(&mut r, source);

        // Tuned at 100.8 MHz, its mirror lands at 100.3 MHz, 30 dB down, at the source's width,
        // and the detector's own mirror test flagged it: an image, not an emission.
        let image = station(
            &mut r,
            sv,
            p,
            100.3e6,
            180e3,
            8.0,
            -48.0,
            tr(1, 4),
            DetectionFlags {
                image_candidate: true,
                ..DetectionFlags::default()
            },
        );
        // A second, weaker box 60 kHz into the confirmed station's skirt: the same emission again.
        let offset = station(
            &mut r,
            sv,
            p,
            101.36e6,
            180e3,
            14.0,
            -30.0,
            tr(1, 5),
            DetectionFlags::default(),
        );
        // A real new emission well clear of everything: no mechanism and no overlap explain it.
        let fresh = station(
            &mut r,
            sv,
            p,
            99.8e6,
            180e3,
            22.0,
            -22.0,
            tr(1, 5),
            DetectionFlags::default(),
        );

        for id in [image, offset, fresh] {
            r.resolve_overlaps(id, "test/overlap@1", t(5), &tol)
                .unwrap();
        }
        // The scene is honest: the rules really did attribute two rows and leave the third alone.
        let kind_of = |r: &Repository, id: EmitterId| {
            r.emitter_relations(id).unwrap().first().map(|x| x.kind)
        };
        assert_eq!(kind_of(&r, image), Some(RelationKind::ArtifactOf));
        assert_eq!(kind_of(&r, offset), Some(RelationKind::SuppressedBy));
        assert_eq!(kind_of(&r, fresh), None, "an independent emission");

        // Watch the whole band.
        let mut sel = Selection::new("FM band", 99.6e6, 102.0e6);
        sel.watch = Some(SelectionWatch { enabled: true });
        let selection_id = sel.id;
        r.insert_selection(&sel).unwrap();

        let svc = service(r);
        let events = svc.observe_watches(
            &[
                sighting(fresh, 99.8e6, 180e3),
                sighting(image, 100.3e6, 180e3),
                sighting(offset, 101.36e6, 180e3),
            ],
            t(10),
        );

        // Exactly one alert, and it is the emission nothing explains.
        assert_eq!(
            events.iter().map(|e| e.alert.emitter).collect::<Vec<_>>(),
            vec![fresh],
            "the image and the suppressed twin must not alert as new activity"
        );
        let alert = &events[0];
        assert_eq!(alert.anomaly.kind, AnomalyKind::NewEmitter);
        assert_eq!(alert.anomaly.subject, AnomalySubject::Emitter(fresh));
        assert_eq!(
            watch::parse_baseline_ref(alert.anomaly.baseline_ref.as_deref().unwrap()),
            Some((selection_id, fresh)),
            "the alert is keyed to the selection whose watch fired"
        );

        // The alert carries its reasoning, in the explanation as well as the record.
        let Cause::OwnHistory { description } = &alert.explanation.cause else {
            panic!("a watch alert must explain itself: {:?}", alert.explanation);
        };
        assert!(
            description.contains("FM band") && description.contains("no standing suppression"),
            "thin reasoning: {description}"
        );

        // The suppressed rows are disclosed, with the relationship that stopped each of them.
        let report = svc.watch_report(selection_id).unwrap();
        assert!(report.armed);
        assert_eq!(report.alerted_total, 1);
        assert_eq!(report.suppressed_total, 2);
        let by_emitter = |id: EmitterId| {
            report
                .skipped
                .iter()
                .find(|s| s.emitter == id)
                .unwrap_or_else(|| panic!("no skip recorded for {id}"))
                .clone()
        };
        let img = by_emitter(image);
        assert_eq!(img.reason, watch::WatchSkipReason::Deferred);
        assert_eq!(
            img.relation.as_ref().map(|x| x.kind),
            Some(RelationKind::ArtifactOf)
        );
        assert!(
            img.text.contains("not new activity") && img.text.contains("100.800000 MHz"),
            "the skip quotes the artifact arithmetic: {}",
            img.text
        );
        let off = by_emitter(offset);
        assert_eq!(
            off.relation.as_ref().map(|x| x.kind),
            Some(RelationKind::SuppressedBy)
        );

        // Told once: the same sightings again raise nothing new.
        let again = svc.observe_watches(&[sighting(fresh, 99.8e6, 180e3)], t(20));
        assert!(again.is_empty(), "one alert per emitter per watch");
    }

    /// A watch is armed and disarmed by the user, and disarming is not destructive: new activity
    /// stops raising alerts, and the alert already raised keeps its row and its reasoning.
    #[test]
    fn disarming_a_watch_stops_new_alerts_and_keeps_the_ones_already_raised() {
        let (mut r, sv) = scene();
        let p = prov(&mut r, 100.8e6);
        let first = station(
            &mut r,
            sv,
            p,
            99.8e6,
            180e3,
            22.0,
            -22.0,
            tr(1, 5),
            DetectionFlags::default(),
        );
        let second = station(
            &mut r,
            sv,
            p,
            100.9e6,
            180e3,
            22.0,
            -22.0,
            tr(1, 5),
            DetectionFlags::default(),
        );
        let mut sel = Selection::new("FM band", 99.6e6, 102.0e6);
        sel.watch = Some(SelectionWatch { enabled: true });
        let id = sel.id;
        r.insert_selection(&sel).unwrap();
        let svc = service(r);

        assert_eq!(
            svc.observe_watches(&[sighting(first, 99.8e6, 180e3)], t(10))
                .len(),
            1
        );

        // Disarm through the store, as the API's PUT does.
        {
            let mut repo = lock(&svc.repo);
            let mut stored = repo.selection(id).unwrap();
            stored.watch = Some(SelectionWatch { enabled: false });
            repo.update_selection(&stored).unwrap();
        }
        assert!(
            svc.observe_watches(&[sighting(second, 100.9e6, 180e3)], t(20))
                .is_empty(),
            "a disarmed watch raises nothing"
        );
        let report = svc.watch_report(id).unwrap();
        assert!(!report.armed);
        assert_eq!(report.alerts.len(), 1, "the earlier alert is kept");
        assert!(!report.alerts[0].reason.is_empty(), "with its reasoning");
    }

    /// An unwatched selection is quiet, and an unknown one is `NotFound`.
    #[test]
    fn an_unarmed_selection_never_alerts() {
        let (mut r, sv) = scene();
        let p = prov(&mut r, 100.8e6);
        let e = station(
            &mut r,
            sv,
            p,
            99.8e6,
            180e3,
            22.0,
            -22.0,
            tr(1, 5),
            DetectionFlags::default(),
        );
        let sel = Selection::new("FM band", 99.6e6, 102.0e6);
        let id = sel.id;
        r.insert_selection(&sel).unwrap();
        let svc = service(r);
        assert!(
            svc.observe_watches(&[sighting(e, 99.8e6, 180e3)], t(10))
                .is_empty()
        );
        assert!(!svc.watch_report(id).unwrap().armed);
        assert_eq!(
            svc.watch_report(SelectionId::new()),
            Err(AlarmFail::NotFound)
        );
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
                look: None,
                sequential: None,
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
                look: None,
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

    /// T-136 (T-131 follow-up): inventory first sightings reach the alarms through the calls the
    /// occupancy close makes (`note_first_sightings` → `ingest_interval` → `new_emitter_inputs` →
    /// `observe_interval_with_new_emitters`). With a matured first-sighting rate (one new emitter
    /// every 12 h), a burst of new emitters on one channel opens exactly one `new-emitter` alarm
    /// carrying that channel and the site, which clears once the burst leaves the window. With an
    /// immature rate the same burst raises nothing and is counted as `immature-baseline`.
    #[test]
    fn alarm_service_new_emitter_alarm_from_first_sightings() {
        use crate::attention::tests::series_row;
        use crate::attention::{AttentionService, FirstSighting, SiteSelect};
        use hk_model::attention::occupancy::OccupancySubject;
        use hk_model::ids::EmitterId;

        let channel = FreqRange::new(432.0e6, 432.0125e6);
        let run = |burst_q: i64| {
            let attention = AttentionService::in_memory().unwrap();
            let cur = attention
                .set_current_site(SiteSelect {
                    name: Some("home".into()),
                    ..SiteSelect::default()
                })
                .unwrap();
            let id: SiteId = cur["record"]["id"].as_str().unwrap().parse().unwrap();
            let site = SiteKey::Site(id);
            let alarms = AlarmService::open(
                Arc::new(Mutex::new(Repository::open_in_memory().unwrap())),
                None,
                Arc::new(|| Timestamp::from_unix_nanos(0)),
            )
            .unwrap();
            let band = OccupancySubject::Band {
                freq: FreqRange::new(431e6, 433e6),
            };
            let mut events = Vec::new();
            for q in 0..burst_q + 12 {
                let row = series_row(q, site, band, 0.1);
                let t_end = row.interval.end;
                let sightings: Vec<FirstSighting> = if q == burst_q {
                    // Six new emitters on one 12.5 kHz channel.
                    (0..6)
                        .map(|i| FirstSighting {
                            emitter: EmitterId::new(),
                            freq: FreqRange::centered(432.00625e6 + f64::from(i) * 100.0, 10e3),
                            t: t_end,
                        })
                        .collect()
                } else if q == burst_q + 1 {
                    // An ordinary sighting on another channel inside the burst's hour: its own
                    // group holds one sighting, so it must not alarm on the burst's count.
                    vec![FirstSighting {
                        emitter: EmitterId::new(),
                        freq: FreqRange::centered(431.5e6, 10e3),
                        t: t_end,
                    }]
                } else if q % 48 == 0 {
                    // The usual rate, elsewhere in the band.
                    vec![FirstSighting {
                        emitter: EmitterId::new(),
                        freq: FreqRange::centered(431.5e6 - q as f64 * 1e3, 10e3),
                        t: t_end,
                    }]
                } else {
                    Vec::new()
                };
                attention.note_first_sightings(site, &sightings);
                let folds = attention.ingest_interval(
                    std::slice::from_ref(&row),
                    sightings.len() as u64,
                    t_end,
                    0,
                );
                let inputs = attention.new_emitter_inputs(site, t_end);
                events.extend(alarms.observe_interval_with_new_emitters(
                    &folds,
                    site,
                    t_end,
                    &inputs,
                    &[],
                ));
            }
            (id, alarms, events)
        };

        let (id, alarms, events) = run(120);
        let raised: Vec<&AlarmEvent> = events
            .iter()
            .filter(|e| e.transition == AlarmLifecycle::Raised)
            .collect();
        assert_eq!(raised.len(), 1, "{events:#?}");
        let e = raised[0];
        assert_eq!(e.row.key.kind, AlarmKind::NewEmitter);
        assert_eq!(e.anomaly.kind, hk_model::AnomalyKind::NewEmitter);
        assert_eq!(e.row.key.site, id);
        assert!(matches!(
            e.row.key.subject,
            AlarmSubject::Cells { scheme: 1, .. }
        ));
        assert!(
            e.row.freq.lo_hz >= channel.lo_hz - 6250.0
                && e.row.freq.hi_hz <= channel.hi_hz + 6250.0,
            "the channel's extent: {:?}",
            e.row.freq
        );
        assert_eq!(e.row.detail.observed, 6.0);
        assert!(
            !events
                .iter()
                .any(|c| c.row.freq.lo_hz <= 431.5e6 && c.row.freq.hi_hz >= 431.5e6),
            "the ordinary sighting beside the burst stays quiet: {events:#?}"
        );
        assert!(
            events
                .iter()
                .any(|c| c.transition == AlarmLifecycle::Cleared
                    && c.row.anomaly_id == e.row.anomaly_id),
            "clears once the burst leaves the window: {events:#?}"
        );
        let (listed, _) = alarms
            .list(&AnomalyQuery {
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(listed.len(), 1, "one alarm for the burst");

        let (_, alarms, events) = run(10);
        assert!(events.is_empty(), "immature rate: {events:#?}");
        let s = alarms.suppressions();
        // Counted once per sighting at its own close: q=0 (1), the burst (6), the ordinary one (1).
        assert_eq!(
            s["new-emitter"]["immature-baseline"].as_u64().unwrap(),
            8,
            "{s}"
        );
        assert_eq!(alarms.status_json()["inputs_mature"], 0);
    }

    /// T-138 (ADR-0012 §7.1 rule "single persistent new emitter"; AWARE-044, AWARE-027): one new
    /// emitter on a quiet mature site (no new emitters for 10 days, P(≥1 new emitter in the hour |
    /// μ) ≤ α) opens exactly one `new-emitter` alarm once it is seen in two consecutive closes, or
    /// again on the next visit. The same emitter on a busy site, a one-off transient, a suspect
    /// emitter and an immature site raise nothing (the immature one is counted).
    #[test]
    fn alarm_service_single_persistent_new_emitter_rule() {
        use crate::attention::tests::series_row;
        use crate::attention::{
            AttentionService, EmitterSeen, FirstSighting, ObservedBand, SiteSelect,
        };
        use hk_context::occupancy::novelty::persistent_single_alpha;
        use hk_model::attention::occupancy::OccupancySubject;
        use hk_model::ids::EmitterId;

        const S: i64 = 1_000_000_000;
        let channel = FreqRange::new(432.0e6, 432.0125e6);
        let emitter_freq = FreqRange::centered(432.00625e6, 10e3);
        /// `build` closes before the sighting; ordinary one-off sightings every `busy` closes; the
        /// emitter seen at `seen_at` offsets from its sighting close (sorted); no closes in `gap`;
        /// `churned` flags it as churn at its sighting; `coarse` offsets observe the band at a
        /// 100 kHz RBW; `tx` fixes its inventory first/last seen (s from the sighting close end).
        #[derive(Clone, Copy, Default)]
        struct Case<'a> {
            build: i64,
            busy: Option<i64>,
            seen_at: &'a [i64],
            gap: (i64, i64),
            suspect: bool,
            churned: bool,
            coarse: &'a [i64],
            tx: Option<(i64, i64)>,
        }
        let run = |c: Case<'_>| {
            let Case {
                build,
                busy,
                seen_at,
                gap,
                suspect,
                churned,
                coarse,
                tx,
            } = c;
            let attention = AttentionService::in_memory().unwrap();
            let cur = attention
                .set_current_site(SiteSelect {
                    name: Some("home".into()),
                    ..SiteSelect::default()
                })
                .unwrap();
            let id: SiteId = cur["record"]["id"].as_str().unwrap().parse().unwrap();
            let site = SiteKey::Site(id);
            let alarms = AlarmService::open(
                Arc::new(Mutex::new(Repository::open_in_memory().unwrap())),
                None,
                Arc::new(|| Timestamp::from_unix_nanos(0)),
            )
            .unwrap();
            let band = FreqRange::new(431e6, 433e6);
            let emitter = EmitterId::new();
            let mut events = Vec::new();
            let mut at_sighting = None;
            let first_seen = series_row(build, site, OccupancySubject::Band { freq: band }, 0.1)
                .interval
                .start
                .saturating_add_nanos(60 * S);
            for q in 0..=build + seen_at.last().copied().unwrap_or(0) + 6 {
                if (gap.0..gap.1).contains(&(q - build)) {
                    continue;
                }
                let row = series_row(q, site, OccupancySubject::Band { freq: band }, 0.1);
                let t_end = row.interval.end;
                let mut sightings = Vec::new();
                let mut seen = Vec::new();
                if q == build {
                    sightings.push(FirstSighting {
                        emitter,
                        freq: emitter_freq,
                        t: t_end,
                    });
                } else if busy.is_some_and(|n| q % n == 0) {
                    let other = EmitterId::new();
                    let freq = FreqRange::centered(431.2e6 + (q % 40) as f64 * 5e3, 10e3);
                    sightings.push(FirstSighting {
                        emitter: other,
                        freq,
                        t: t_end,
                    });
                    seen.push(EmitterSeen {
                        emitter: other,
                        freq,
                        first_seen: t_end.saturating_add_nanos(-120 * S),
                        last_seen: t_end.saturating_add_nanos(-100 * S),
                        suspect: false,
                    });
                }
                if q >= build && seen_at.contains(&(q - build)) {
                    let build_end = t_end.saturating_add_nanos(-(q - build) * 900 * S);
                    let (first_seen, last_seen) = tx.map_or(
                        (first_seen, t_end.saturating_add_nanos(-30 * S)),
                        |(f, l)| {
                            (
                                build_end.saturating_add_nanos(f * S),
                                build_end.saturating_add_nanos(l * S),
                            )
                        },
                    );
                    seen.push(EmitterSeen {
                        emitter,
                        freq: emitter_freq,
                        first_seen,
                        last_seen,
                        suspect,
                    });
                }
                attention.note_first_sightings(site, &sightings);
                let rbw_hz = if q >= build && coarse.contains(&(q - build)) {
                    100e3
                } else {
                    6250.0
                };
                let churn = if churned && q == build {
                    vec![emitter]
                } else {
                    Vec::new()
                };
                attention.note_emitters_seen(
                    site,
                    row.interval,
                    &[ObservedBand { freq: band, rbw_hz }],
                    &seen,
                    &churn,
                );
                let folds = attention.ingest_interval(
                    std::slice::from_ref(&row),
                    sightings.len() as u64,
                    t_end,
                    0,
                );
                let inputs = attention.new_emitter_inputs(site, t_end);
                if q == build {
                    at_sighting = inputs
                        .iter()
                        .find(|n| n.input.freq.lo_hz <= emitter_freq.center_hz())
                        .filter(|n| n.input.freq.hi_hz >= emitter_freq.center_hz())
                        .map(|n| (n.input.novelty, n.input.baseline_mean));
                }
                events.extend(alarms.observe_interval_with_new_emitters(
                    &folds,
                    site,
                    t_end,
                    &inputs,
                    &[],
                ));
            }
            (id, alarms, events, at_sighting)
        };
        let alpha = persistent_single_alpha(0.7);
        let raised = |events: &[AlarmEvent]| -> Vec<AlarmEvent> {
            events
                .iter()
                .filter(|e| e.transition == AlarmLifecycle::Raised)
                .cloned()
                .collect()
        };
        let count = |events: &[AlarmEvent], t: AlarmLifecycle| {
            events.iter().filter(|e| e.transition == t).count()
        };
        // 10 days of 15-min closes without a new emitter.
        let quiet = Case {
            build: 960,
            seen_at: &[0, 1, 2, 3],
            ..Case::default()
        };

        // Quiet mature site, persistent emitter: exactly one alarm with channel, site, explanation.
        let (id, alarms, events, at) = run(quiet);
        let (novelty, mu) = at.expect("scored at its sighting close");
        let p1 = -(-mu).exp_m1();
        eprintln!("quiet: novelty {novelty:.3}, μ {mu:.5}, P(≥1) {p1:.5}, α {alpha:.5}");
        assert!(novelty >= 0.7 && p1 <= alpha, "novelty {novelty}, P {p1}");
        let r = raised(&events);
        assert_eq!(r.len(), 1, "{events:#?}");
        let e = &r[0];
        assert_eq!(e.row.key.kind, AlarmKind::NewEmitter);
        assert_eq!(e.anomaly.kind, hk_model::AnomalyKind::NewEmitter);
        assert_eq!(e.row.key.site, id);
        assert!(
            e.row.freq.lo_hz >= channel.lo_hz - 6250.0
                && e.row.freq.hi_hz <= channel.hi_hz + 6250.0,
            "the channel's extent: {:?}",
            e.row.freq
        );
        assert_eq!(e.row.detail.observed, 1.0, "one new emitter");
        assert_eq!(e.row.detail.intervals_above, 2);
        assert!(!e.explanations.is_empty(), "explained (unexplained ranked)");
        let (listed, _) = alarms
            .list(&AnomalyQuery {
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(listed.len(), 1);

        // Re-seen on a later visit (no closes at the site for 2 days in between): one alarm.
        let (_, _, events, _) = run(Case {
            seen_at: &[0, 192],
            gap: (1, 192),
            ..quiet
        });
        assert_eq!(raised(&events).len(), 1, "later visit: {events:#?}");

        // T-138 review: one 10-min transmission straddling the sighting close's end shows at two
        // closes but is never a full interval long: no alarm.
        let (_, _, events, _) = run(Case {
            seen_at: &[0, 1],
            tx: Some((-300, 300)),
            ..quiet
        });
        assert!(raised(&events).is_empty(), "straddle: {events:#?}");

        // T-138 review: one-shot. An intermittent repeater (on for 3 h, back 7 h later for 45 min)
        // raises once and is never re-raised or re-opened.
        let (_, _, events, _) = run(Case {
            seen_at: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 40, 41, 42],
            ..quiet
        });
        assert_eq!(count(&events, AlarmLifecycle::Raised), 1, "{events:#?}");
        assert_eq!(count(&events, AlarmLifecycle::Reopened), 0, "{events:#?}");
        assert_eq!(count(&events, AlarmLifecycle::Cleared), 1, "{events:#?}");

        // T-138 review: churn (a new id over an emitter seen within the horizon) never scores.
        let (_, _, events, _) = run(Case {
            churned: true,
            ..quiet
        });
        assert!(events.is_empty(), "churn: {events:#?}");

        // T-138 review: a coarse sweep row (100 kHz bins over a 10 kHz emitter) between two
        // sightings neither resets nor confirms; a resolving row without it resets the streak.
        let coarse_between = [1, 2, 3, 4, 5, 6, 7, 8, 9, 11];
        let (_, _, events, _) = run(Case {
            seen_at: &[0, 10, 12],
            coarse: &coarse_between,
            ..quiet
        });
        assert_eq!(raised(&events).len(), 1, "coarse gap: {events:#?}");
        let (_, _, events, _) = run(Case {
            seen_at: &[0, 10, 12],
            coarse: &coarse_between[..9],
            ..quiet
        });
        assert!(raised(&events).is_empty(), "resolved gap: {events:#?}");

        // Busy site (a new emitter every 2 h): the same persistent emitter does not alarm.
        let (_, _, events, at) = run(Case {
            busy: Some(8),
            ..quiet
        });
        let (novelty, mu) = at.unwrap();
        let p1 = -(-mu).exp_m1();
        eprintln!("busy: novelty {novelty:.3}, μ {mu:.5}, P(≥1) {p1:.5}");
        assert!(novelty < 0.7 && p1 > alpha);
        assert!(events.is_empty(), "busy: {events:#?}");

        // One-off transient on the quiet site: seen at its sighting close only.
        let (_, _, events, at) = run(Case {
            seen_at: &[0],
            ..quiet
        });
        assert!(at.unwrap().0 >= 0.7, "scored once, not confirmed");
        assert!(events.is_empty(), "transient: {events:#?}");

        // Suspect (e.g. spur/IMD) sightings never score.
        let (_, _, events, _) = run(Case {
            suspect: true,
            ..quiet
        });
        assert!(events.is_empty(), "suspect: {events:#?}");

        // Immature baseline: no alarm, the sighting counted once as `immature-baseline`.
        let (_, alarms, events, _) = run(Case { build: 10, ..quiet });
        assert!(events.is_empty(), "immature: {events:#?}");
        let s = alarms.suppressions();
        assert_eq!(
            s["new-emitter"]["immature-baseline"].as_u64().unwrap(),
            1,
            "{s}"
        );
    }

    /// T-146 (ADR-0012 §7.2): a time-compressed sparse-visit run through the live service path
    /// (`BaselineEngine::observe` → `observe_interval`). A channel observed with two effective
    /// looks per 15-min interval at FCO ≈ 0.05 builds a mature baseline over 26 h, then goes
    /// fully busy. No single interval is novel enough, but the accumulated evidence raises one
    /// busier-than-usual alarm carrying the site, the channel and its explanation (with the
    /// sequential evidence).
    ///
    /// The service raises exactly where the §7.2 rule says: from the run the engine holds just
    /// before onset (here the last baseline look, FCO 0.5, already counts as a busier interval)
    /// plus each onset interval's z, the first second-consecutive interval whose sequential
    /// novelty is ≥ on. So every interval is counted once and nothing resets the run on the
    /// service path. The a-priori latency bound for a given z is the hk-context sparse onset
    /// test's; this scene's pool gives z ≈ 3.09, diluted by the pre-onset look.
    #[test]
    fn alarm_service_sparse_visits_raise_busier_than_usual() {
        use crate::attention::IntervalFold;
        use hk_context::occupancy::baseline::{BaselineConfig, BaselineEngine};
        use hk_context::occupancy::novelty::{SEQUENTIAL_MAX_GAP_S, sequential_step};
        use hk_model::attention::alarm::HysteresisConfig;
        use hk_model::attention::baseline::{BaselineKey, ChainKey};
        use hk_model::attention::occupancy::{ChannelKey, OccupancySubject};
        use hk_model::{Cause, Evidence};
        use hk_store::baseline::{BaselineState, BaselineSubject};

        const BASELINE: i64 = 26 * 4;
        let (hcfg, ncfg) = (HysteresisConfig::default(), NoveltyConfig::default());
        let repo = Arc::new(Mutex::new(Repository::open_in_memory().unwrap()));
        let svc = AlarmService::open(Arc::clone(&repo), None, Arc::new(|| at(0))).unwrap();
        let site_id = SiteId::new();
        let site = SiteKey::Site(site_id);
        let key = ChannelKey {
            scheme: 1,
            lo_cell: 69_355,
            hi_cell: 69_357,
        };
        let mut baseline = BaselineEngine::new(
            BaselineState::new(
                BaselineKey {
                    site: site_id,
                    cal: CalKey::Uncalibrated,
                    chain: ChainKey::Unknown,
                    scheme: 1,
                    cell_factor: 16,
                },
                at(0),
            ),
            0,
            BaselineConfig::default(),
        );
        let mut state = 0x146d_u64;
        let mut next = move || {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
        };
        let mut raised = None;
        // The rule's prediction: (k, Σz) of the busier run, whether the last interval was on,
        // the predicted raise (interval, k), and the pre-onset run.
        let (mut run, mut prev_on, mut predicted, mut pre) = (None, false, None, None);
        for i in 0..BASELINE + 60 {
            let fco = if i < BASELINE {
                (0..2).filter(|_| next() < 0.05).count() as f64 / 2.0
            } else {
                1.0
            };
            let obs = IntervalObservation {
                subject: BaselineSubject::Channel { key },
                t: at(i),
                gain: 0,
                level_db: None,
                max_db: None,
                occupied_weight_s: fco * 900.0,
                weight_s: 900.0,
                observed_s: 900.0,
                n_eff: 2.0,
                suspect_fraction: 0.0,
                provenance_explained: false,
            };
            let fold = baseline.observe(&obs);
            let f = IntervalFold {
                subject: OccupancySubject::Channel { key },
                site,
                cal: CalKey::Uncalibrated,
                obs,
                utc_offset_min: 0,
                pool: PoolContext::default(),
                fold,
            };
            let events = svc.observe_interval(&[f], &[]);
            if i < BASELINE {
                assert!(events.is_empty(), "baseline interval {i}: {events:?}");
                if i == BASELINE - 1 {
                    let r = lock(&svc.engine).sequential_run(
                        site_id,
                        AlarmSubject::Channel { key },
                        CalKey::Uncalibrated,
                    );
                    run = r;
                    pre = Some(r.map(|r| (r.direction, r.k, r.sum)));
                }
                continue;
            }
            let z = fold.novelty.occupancy_z.expect("a mature fold is scored");
            assert!(z > 0.0, "interval {i}: busier ({z})");
            let look = fold.look.expect("a scored fold carries its look");
            let ev = sequential_step(&mut run, at(i), z, &look, SEQUENTIAL_MAX_GAP_S, &ncfg);
            let on = ev.is_some_and(|e| e.z > 0.0 && e.novelty >= hcfg.on);
            if on && prev_on && predicted.is_none() {
                predicted = Some((i - BASELINE + 1, ev.unwrap().intervals, look.busier_start));
            }
            prev_on = on;
            assert!(
                fold.novelty.novelty < 0.4,
                "interval {i}: one interval alone is not novel ({:?})",
                fold.novelty
            );
            if let Some(e) = events
                .into_iter()
                .find(|e| e.transition == AlarmLifecycle::Raised)
            {
                raised = Some((i - BASELINE + 1, e));
                break;
            }
        }
        let (latency, e) = raised.expect("the sparse busy channel raises");
        let (want, k, z) = predicted.expect("the rule predicts a raise within the scene");
        println!(
            "T-146 service sparse onset: per-interval z {z:.3}, pre-onset run {pre:?}, raised at \
             interval {latency} (rule: {want}, run k {k})"
        );
        assert_eq!(latency, want, "the service raises where the rule does");
        assert_eq!(e.row.key.kind, AlarmKind::BusierThanUsual);
        assert_eq!(e.row.key.site, site_id);
        assert_eq!(e.row.key.subject, AlarmSubject::Channel { key });
        assert_eq!(e.anomaly.region.freq.lo_hz, 69_355.0 * SCHEME_1_CELL_HZ);
        assert_eq!(e.anomaly.region.freq.hi_hz, 69_357.0 * SCHEME_1_CELL_HZ);
        let top = e.explanations.first().expect("explained");
        assert_eq!(top.cause, Cause::Unexplained);
        let intervals = top.evidence.iter().find_map(|v| match v {
            Evidence::Value { name, value } if name == "sequential_intervals" => Some(*value),
            _ => None,
        });
        assert_eq!(intervals, Some(f64::from(k)));
        let status = svc.status_json();
        assert_eq!(status["open"], 1, "{status}");
    }
}
