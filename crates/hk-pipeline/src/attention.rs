//! Baseline/novelty/score wiring (T-119, ADR-0012 §3–§4): runs the C12 scoring loop and publishes
//! `CandidateSet` snapshots to the scheduler's `InterestingnessProvider`.
//!
//! [`AttentionService`] owns, for one run:
//! - the site state machine (`hk_context::occupancy::site`), persisted to the run database;
//! - the baselines (`hk_context::occupancy::baseline`) over `<data>/baselines` (hk-store);
//! - the score weights (versioned rows, migration 0002) and the [`SharedInterestingness`] the
//!   scheduler (T-120) reads;
//! - the JSON views the control API serves (`hk_api::attention`, adapted in hk-cli).
//!
//! **Inputs.** [`AttentionService::ingest_occupancy`] is the thin adapter for T-118's
//! `OccupancyStat` rows (called once per interval close); [`AttentionService::observe`] takes a
//! prepared fold (history cells, tests); [`AttentionService::on_fix`] takes C06 fixes;
//! [`AttentionService::score`] ranks measured candidates. Until T-118 lands nothing in the
//! pipeline calls the inputs, so a served run has sites, weights and empty baselines/candidates.
//!
//! All contract time is the sample clock handed in by the caller (ADR-0012 §0); the service's
//! `clock` (stream time) is used only to stamp API-created sites and weight rows.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use hk_context::occupancy::baseline::{
    BaselineConfig, BaselineCopy, Baselines, FoldOutcome, IntervalObservation, PoolStats,
    from_occupancy_stat, in_pool, maturity, pools,
};
use hk_context::occupancy::score::{CandidateInput, Scorer};
use hk_context::occupancy::site::{Fix, SiteAssigner};
use hk_model::Repository;
use hk_model::attention::baseline::{
    BaselineResolution, CalKey, HourOfWeek, Maturity, SiteConfig, SiteKey, SiteRecord, SiteSource,
};
use hk_model::attention::occupancy::OccupancyStat;
use hk_model::attention::score::{InterestingnessProvider, ScoreWeights, SharedInterestingness};
use hk_model::ids::SiteId;
use hk_model::time::Timestamp;
use hk_store::baseline::{BaselineStore, BaselineSubject};
use serde_json::{Value, json};

use crate::stats::Counters;

/// Level-0 cell width of history scheme 1, Hz.
pub const SCHEME_1_CELL_HZ: f64 = 6250.0;
/// Baseline cell factor (100 kHz on scheme 1).
pub const CELL_FACTOR: u16 = 16;
/// Most subjects `/api/baselines/slots` returns.
pub const SLOTS_MAX: usize = 2_000;
/// Default and maximum candidates `/api/candidates` returns.
pub const CANDIDATES_DEFAULT: usize = 100;
/// Maximum candidates `/api/candidates` returns.
pub const CANDIDATES_MAX: usize = 1_000;

/// A refused or failed attention call: HTTP status, stable code, message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionError {
    /// HTTP status.
    pub status: u16,
    /// Stable code (`invalid`, `not_found`, `conflict`, `failed`).
    pub code: &'static str,
    /// Message (never echoes values).
    pub message: String,
}

impl AttentionError {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(400, "invalid", message)
    }

    fn failed(what: &str, e: impl std::fmt::Display) -> Self {
        Self::new(500, "failed", format!("{what}: {e}"))
    }
}

/// What `PUT /api/sites/current` asks for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SiteSelect {
    /// Select a known site.
    pub id: Option<SiteId>,
    /// Select the site of this name, or create it.
    pub name: Option<String>,
    /// Centroid of a created site.
    pub lat_deg: Option<f64>,
    /// Centroid of a created site.
    pub lon_deg: Option<f64>,
    /// Radius of a created site, m.
    pub radius_m: Option<f64>,
    /// UTC offset of a created site, minutes.
    pub utc_offset_min: Option<i16>,
    /// Release the pin (fixes decide again).
    pub release: bool,
}

/// C12 for one run.
pub struct AttentionService {
    repo: Arc<Mutex<Repository>>,
    sites: Mutex<SiteAssigner>,
    baselines: Mutex<Baselines>,
    scorer: Mutex<Scorer>,
    weights: Mutex<ScoreWeights>,
    provider: Arc<SharedInterestingness>,
    counters: Option<Arc<Counters>>,
    clock: Arc<dyn Fn() -> Timestamp + Send + Sync>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn secs(t: Timestamp) -> f64 {
    t.as_unix_nanos() as f64 / 1e9
}

fn subject_extent(s: &BaselineSubject) -> (f64, f64) {
    match s {
        BaselineSubject::Cell { index } => {
            let w = SCHEME_1_CELL_HZ * f64::from(CELL_FACTOR);
            (*index as f64 * w, (*index + 1) as f64 * w)
        }
        BaselineSubject::Channel { key } => (
            key.lo_cell as f64 * SCHEME_1_CELL_HZ,
            key.hi_cell as f64 * SCHEME_1_CELL_HZ,
        ),
    }
}

fn site_json(s: &SiteRecord) -> Value {
    json!({
        "id": s.id.to_string(),
        "name": s.name,
        "lat_deg": s.lat_deg,
        "lon_deg": s.lon_deg,
        "radius_m": s.radius_m,
        "utc_offset_min": s.utc_offset_min,
        "source": s.source,
        "first_seen": secs(s.first_seen),
        "last_seen": secs(s.last_seen),
        "observed_s": s.observed_s,
    })
}

fn opt_finite(v: Option<f64>) -> Value {
    v.filter(|x| x.is_finite())
        .map_or(Value::Null, |x| json!(x))
}

fn pool_json(p: &PoolStats, resolution: BaselineResolution) -> Value {
    json!({
        "resolution": resolution,
        "n": p.n,
        "observed_s": p.observed_s,
        "mean_db": opt_finite(p.mean_db()),
        "std_db": opt_finite(p.std_db()),
        "fco": opt_finite(p.fco()),
        "max_db": opt_finite(Some(p.max_db)),
    })
}

impl AttentionService {
    /// Opens the service over `data_dir/baselines` and the run database (sites and weights).
    pub fn open(
        data_dir: &Path,
        repo: Arc<Mutex<Repository>>,
        counters: Option<Arc<Counters>>,
        clock: Arc<dyn Fn() -> Timestamp + Send + Sync>,
    ) -> anyhow::Result<Self> {
        let (sites, weights) = {
            let r = lock(&repo);
            (r.sites()?, r.score_weights()?)
        };
        let store = BaselineStore::open(data_dir.join("baselines"))?;
        Ok(Self {
            repo,
            sites: Mutex::new(SiteAssigner::new(SiteConfig::default(), sites)),
            baselines: Mutex::new(Baselines::new(
                BaselineConfig::default(),
                1,
                CELL_FACTOR,
                Some(store),
            )),
            scorer: Mutex::new(Scorer::default()),
            weights: Mutex::new(weights),
            provider: Arc::new(SharedInterestingness::default()),
            counters,
            clock,
        })
    }

    /// The provider the scheduler reads (T-120).
    pub fn provider(&self) -> Arc<SharedInterestingness> {
        Arc::clone(&self.provider)
    }

    fn bump(&self, f: impl FnOnce(&Counters)) {
        if let Some(c) = &self.counters {
            f(c);
        }
    }

    fn persist_sites(&self, sites: &mut SiteAssigner) {
        let dirty = sites.take_dirty();
        if dirty.is_empty() {
            return;
        }
        let repo = lock(&self.repo);
        for s in dirty {
            if repo.upsert_site(&s).is_err() {
                self.bump(|c| {
                    c.attention.errors.fetch_add(1, Ordering::Relaxed);
                });
            }
        }
    }

    /// Folds a C06 fix.
    pub fn on_fix(&self, fix: Fix) -> SiteKey {
        let mut sites = lock(&self.sites);
        let key = sites.on_fix(fix);
        self.persist_sites(&mut sites);
        key
    }

    /// Folds one prepared observation under the current site.
    pub fn observe(&self, cal: CalKey, obs: &IntervalObservation) -> Option<FoldOutcome> {
        let (site, offset) = {
            let mut sites = lock(&self.sites);
            let site = sites.tick(obs.t);
            if site.accrues_baseline() {
                sites.record_observation(obs.t, obs.observed_s);
            }
            (site, sites.utc_offset_min(site))
        };
        let out = {
            let mut b = lock(&self.baselines);
            let out = b.observe(site, offset, cal, obs);
            if let Ok(n) = b.flush(obs.t, false) {
                self.bump(|c| {
                    c.attention
                        .baseline_writes
                        .fetch_add(n as u64, Ordering::Relaxed);
                });
            }
            out
        };
        let mut sites = lock(&self.sites);
        self.persist_sites(&mut sites);
        match out {
            Ok(o) => {
                self.bump(|c| {
                    c.attention.folds.fetch_add(1, Ordering::Relaxed);
                    if o.novelty.novelty > 0.0 {
                        c.attention.novel_folds.fetch_add(1, Ordering::Relaxed);
                    }
                    if o.change_point.is_some() {
                        c.attention.change_points.fetch_add(1, Ordering::Relaxed);
                    }
                });
                Some(o)
            }
            Err(_) => {
                self.bump(|c| {
                    c.attention.errors.fetch_add(1, Ordering::Relaxed);
                });
                None
            }
        }
    }

    /// T-118 adapter: folds one `OccupancyStat` row. The row's own `site` is the assignment at
    /// measurement time; a row whose site does not accrue folds nothing.
    pub fn ingest_occupancy(&self, stat: &OccupancyStat, gain: u32) -> Option<FoldOutcome> {
        let (site, cal, obs) = from_occupancy_stat(stat, gain)?;
        let offset = lock(&self.sites).utc_offset_min(site);
        let mut b = lock(&self.baselines);
        let out = b.observe(site, offset, cal, &obs).ok();
        let _ = b.flush(obs.t, false);
        out
    }

    /// Scores `inputs` at `t` and publishes when due and changed.
    pub fn score(&self, t: Timestamp, low_power: bool, inputs: &[CandidateInput]) -> Option<u64> {
        let site = lock(&self.sites).current();
        let w = *lock(&self.weights);
        let published = lock(&self.scorer)
            .run(t, low_power, site, w, inputs, &self.provider)
            .ok()
            .flatten();
        if published.is_some() {
            self.bump(|c| {
                c.attention.publishes.fetch_add(1, Ordering::Relaxed);
            });
        }
        published
    }

    /// Saves every dirty baseline (shutdown / low battery).
    pub fn checkpoint(&self, t: Timestamp) {
        let _ = lock(&self.baselines).flush(t, true);
    }

    // ---- API views ----

    /// `GET /api/sites`.
    pub fn sites_json(&self) -> Value {
        let s = lock(&self.sites);
        json!({
            "sites": s.sites().iter().map(site_json).collect::<Vec<_>>(),
            "current": s.current(),
        })
    }

    /// `GET /api/sites/current`.
    pub fn current_site_json(&self) -> Value {
        let s = lock(&self.sites);
        let record = match s.current() {
            SiteKey::Site(id) => s.site(id).map_or(Value::Null, site_json),
            _ => Value::Null,
        };
        json!({
            "site": s.current(),
            "set_by": s.set_by(),
            "pinned": s.is_pinned(),
            "accrues_baseline": s.current().accrues_baseline(),
            "record": record,
        })
    }

    /// `PUT /api/sites/current`.
    pub fn set_current_site(&self, sel: SiteSelect) -> Result<Value, AttentionError> {
        let t = (self.clock)();
        {
            let mut sites = lock(&self.sites);
            if sel.release {
                sites.unpin();
            } else {
                let id = match (sel.id, &sel.name) {
                    (Some(id), None) => id,
                    (None, Some(name)) => {
                        match sites
                            .sites()
                            .iter()
                            .find(|s| s.name.as_deref() == Some(name))
                        {
                            Some(s) => s.id,
                            None => {
                                let record = SiteRecord {
                                    id: SiteId::new(),
                                    name: Some(name.clone()),
                                    lat_deg: sel.lat_deg,
                                    lon_deg: sel.lon_deg,
                                    radius_m: sel
                                        .radius_m
                                        .unwrap_or(SiteConfig::default().radius_m),
                                    utc_offset_min: sel.utc_offset_min.unwrap_or(0),
                                    source: SiteSource::User,
                                    first_seen: t,
                                    last_seen: t,
                                    observed_s: 0.0,
                                };
                                let id = record.id;
                                sites
                                    .upsert(record)
                                    .map_err(|e| AttentionError::invalid(e.to_string()))?;
                                id
                            }
                        }
                    }
                    _ => {
                        return Err(AttentionError::invalid(
                            "give exactly one of id or name (or release: true)",
                        ));
                    }
                };
                sites
                    .pin(id, SiteSource::User, t)
                    .map_err(|_| AttentionError::new(404, "not_found", "no such site"))?;
            }
            self.persist_sites(&mut sites);
        }
        Ok(self.current_site_json())
    }

    /// `PUT /api/sites/{id}`: rename (`Some(None)` clears) and/or set the UTC offset.
    pub fn update_site(
        &self,
        id: SiteId,
        name: Option<Option<String>>,
        utc_offset_min: Option<i16>,
    ) -> Result<(Value, Value), AttentionError> {
        let mut sites = lock(&self.sites);
        let old = sites
            .site(id)
            .cloned()
            .ok_or_else(|| AttentionError::new(404, "not_found", "no such site"))?;
        if let Some(Some(n)) = &name
            && sites
                .sites()
                .iter()
                .any(|s| s.id != id && s.name.as_deref() == Some(n.as_str()))
        {
            return Err(AttentionError::new(
                409,
                "conflict",
                "another site has that name",
            ));
        }
        let mut new = old.clone();
        if let Some(n) = name {
            new.name = n;
        }
        if let Some(o) = utc_offset_min {
            new.utc_offset_min = o;
        }
        sites
            .upsert(new.clone())
            .map_err(|e| AttentionError::invalid(e.to_string()))?;
        self.persist_sites(&mut sites);
        Ok((site_json(&old), site_json(&new)))
    }

    fn site_or_current(&self, site: Option<SiteId>) -> Result<(SiteId, i16), AttentionError> {
        let s = lock(&self.sites);
        let id = match (site, s.current()) {
            (Some(id), _) | (None, SiteKey::Site(id)) => id,
            (None, _) => {
                return Err(AttentionError::new(
                    409,
                    "conflict",
                    "no current site: give site=<id> (mobile and unassigned build no baselines)",
                ));
            }
        };
        let offset = s
            .site(id)
            .ok_or_else(|| AttentionError::new(404, "not_found", "no such site"))?
            .utc_offset_min;
        Ok((id, offset))
    }

    /// `GET /api/baselines`.
    pub fn baselines_json(&self, site: Option<SiteId>) -> Result<Value, AttentionError> {
        let (id, offset) = self.site_or_current(site)?;
        let now = (self.clock)();
        let slot = HourOfWeek::of(now, offset);
        let mut b = lock(&self.baselines);
        b.load_site(id, offset)
            .map_err(|e| AttentionError::failed("baseline store", e))?;
        let keys: Vec<Value> = b
            .engines()
            .filter(|e| e.state.key.site == id)
            .map(|e| {
                let mut finest: Option<BaselineResolution> = None;
                let mut mature = 0;
                let mut change_points = Vec::new();
                for (subject, sub) in &e.state.subjects {
                    if let Maturity::Mature { resolution } = maturity(sub, slot) {
                        mature += 1;
                        finest = Some(finest.map_or(resolution, |f| f.min(resolution)));
                    }
                    if let Some(cp) = sub.change_point {
                        let (lo, hi) = subject_extent(subject);
                        change_points.push(json!({
                            "subject": subject, "f_lo": lo, "f_hi": hi, "t": secs(cp.t),
                            "statistic": cp.statistic, "direction": cp.direction, "cusum": cp.cusum,
                        }));
                    }
                }
                json!({
                    "site": id.to_string(),
                    "cal": e.state.key.cal,
                    "scheme": e.state.key.scheme,
                    "cell_factor": e.state.key.cell_factor,
                    "subjects": e.state.subjects.len(),
                    "mature_subjects": mature,
                    "finest_resolution": finest,
                    "last_visit": secs(e.state.last_visit),
                    "change_points": change_points,
                })
            })
            .collect();
        Ok(json!({ "site": id.to_string(), "slot": slot.index(), "baselines": keys }))
    }

    /// `GET /api/baselines/slots`.
    pub fn slots_json(
        &self,
        site: Option<SiteId>,
        f_lo: f64,
        f_hi: f64,
        slot: Option<HourOfWeek>,
        resolution: Option<BaselineResolution>,
    ) -> Result<Value, AttentionError> {
        let (id, offset) = self.site_or_current(site)?;
        let slot = slot.unwrap_or_else(|| HourOfWeek::of((self.clock)(), offset));
        let mut b = lock(&self.baselines);
        b.load_site(id, offset)
            .map_err(|e| AttentionError::failed("baseline store", e))?;
        let mut rows = Vec::new();
        let mut truncated = false;
        for e in b.engines().filter(|e| e.state.key.site == id) {
            for (subject, sub) in &e.state.subjects {
                let (lo, hi) = subject_extent(subject);
                if hi <= f_lo || lo >= f_hi {
                    continue;
                }
                if rows.len() >= SLOTS_MAX {
                    truncated = true;
                    break;
                }
                let m = maturity(sub, slot);
                let res = resolution.unwrap_or(match m {
                    Maturity::Mature { resolution } => resolution,
                    Maturity::Immature { .. } => BaselineResolution::AllHours,
                });
                let r = BaselineResolution::FINEST_FIRST
                    .iter()
                    .position(|x| *x == res)
                    .expect("listed");
                // Level from the most-visited gain state; occupancy over all.
                let gain = (0..sub.gains.len()).max_by_key(|gi| {
                    sub.gains[*gi]
                        .reference
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| in_pool(*i, slot, res))
                        .map(|(_, s)| s.n_visits)
                        .sum::<u64>()
                });
                let (rl, ro) = pools(sub, slot, gain, BaselineCopy::Reference);
                let (al, ao) = pools(sub, slot, gain, BaselineCopy::Adaptive);
                let merge = |l: &PoolStats, o: &PoolStats| l.with_occupancy_of(o);
                rows.push(json!({
                    "subject": subject,
                    "f_lo": lo,
                    "f_hi": hi,
                    "cal": e.state.key.cal,
                    "maturity": m,
                    "mixed": sub.mixed,
                    "gain_states": sub.gains.len(),
                    "reference": pool_json(&merge(&rl[r], &ro[r]), res),
                    "adaptive": pool_json(&merge(&al[r], &ao[r]), res),
                    "change_point": sub.change_point.map(|cp| json!({
                        "t": secs(cp.t), "statistic": cp.statistic,
                        "direction": cp.direction, "cusum": cp.cusum,
                    })),
                    "refrozen_at": sub.refrozen_at.map(secs),
                }));
            }
        }
        Ok(json!({
            "site": id.to_string(),
            "slot": slot.index(),
            "subjects": rows,
            "truncated": truncated,
        }))
    }

    /// `POST /api/baselines/refreeze`: copies adaptive → reference for the site's subjects in
    /// `[f_lo, f_hi)` (all when absent). Returns `(response, count)`.
    pub fn refreeze(
        &self,
        site: Option<SiteId>,
        f_lo: Option<f64>,
        f_hi: Option<f64>,
    ) -> Result<Value, AttentionError> {
        let (id, offset) = self.site_or_current(site)?;
        let t = (self.clock)();
        let (lo, hi) = (f_lo.unwrap_or(f64::MIN), f_hi.unwrap_or(f64::MAX));
        let mut b = lock(&self.baselines);
        b.load_site(id, offset)
            .map_err(|e| AttentionError::failed("baseline store", e))?;
        let mut n = 0;
        for e in b.engines_mut().filter(|e| e.state.key.site == id) {
            n += e.refreeze(t, |s| {
                let (a, z) = subject_extent(s);
                z > lo && a < hi
            });
        }
        b.flush(t, true)
            .map_err(|e| AttentionError::failed("baseline store", e))?;
        Ok(json!({ "site": id.to_string(), "refrozen": n }))
    }

    /// `GET /api/candidates`.
    pub fn candidates_json(&self, f_lo: Option<f64>, f_hi: Option<f64>, limit: usize) -> Value {
        let set = self.provider.snapshot();
        let (lo, hi) = (f_lo.unwrap_or(f64::MIN), f_hi.unwrap_or(f64::MAX));
        let matching: Vec<_> = set
            .candidates
            .iter()
            .filter(|c| c.freq.hi_hz > lo && c.freq.lo_hz < hi)
            .collect();
        let truncated = matching.len() > limit;
        let candidates: Vec<Value> = matching
            .into_iter()
            .take(limit)
            .map(|c| {
                let mut v = serde_json::to_value(c).unwrap_or(Value::Null);
                if let Some(eta) = c.next_burst_eta {
                    v["next_burst_eta"] = json!(secs(eta));
                }
                v
            })
            .collect();
        json!({
            "version": self.provider.version(),
            "t": secs(set.t),
            "site": set.site,
            "weights": set.weights,
            "candidates": candidates,
            "truncated": truncated,
        })
    }

    /// `GET /api/attention/weights`.
    pub fn weights_json(&self) -> Result<Value, AttentionError> {
        let history = lock(&self.repo)
            .score_weights_history()
            .map_err(|e| AttentionError::failed("weights store", e))?;
        Ok(json!({
            "weights": *lock(&self.weights),
            "defaults": ScoreWeights::default(),
            "history": history.iter().map(|v| json!({
                "version": v.weights.version,
                "created": secs(v.created_at),
                "author": v.author,
            })).collect::<Vec<_>>(),
        }))
    }

    /// `PUT /api/attention/weights`: stores the next version; returns `(old, new)`. Takes effect at
    /// the next scoring pass.
    pub fn set_weights(
        &self,
        w: ScoreWeights,
        author: &str,
    ) -> Result<(ScoreWeights, ScoreWeights), AttentionError> {
        let probe = ScoreWeights { version: 1, ..w };
        probe
            .validate()
            .map_err(|e| AttentionError::invalid(e.to_string()))?;
        let new = lock(&self.repo)
            .insert_score_weights(&probe, author, (self.clock)())
            .map_err(|e| AttentionError::failed("weights store", e))?;
        let old = std::mem::replace(&mut *lock(&self.weights), new);
        Ok((old, new))
    }
}

#[cfg(test)]
mod tests {
    use hk_context::occupancy::score::BoringEvidence;
    use hk_model::attention::baseline::BaselineResolution;
    use hk_model::attention::score::{CandidateSubject, NoveltyScore};
    use hk_model::region::FreqRange;
    use hk_store::baseline::BaselineSubject;

    use super::*;

    fn service(dir: &Path) -> AttentionService {
        let repo = Arc::new(Mutex::new(Repository::open_in_memory().unwrap()));
        AttentionService::open(
            dir,
            repo,
            Some(Arc::new(Counters::default())),
            Arc::new(|| Timestamp::from_unix_nanos(7 * 86_400 * 1_000_000_000)),
        )
        .unwrap()
    }

    #[test]
    fn attention_service_sites_weights_baselines_and_candidates() {
        let dir = std::env::temp_dir().join(format!("hk-t119-svc-{}", SiteId::new()));
        let s = service(&dir);
        assert_eq!(s.current_site_json()["site"]["kind"], "unassigned");
        assert_eq!(s.baselines_json(None).unwrap_err().status, 409);
        let cur = s
            .set_current_site(SiteSelect {
                name: Some("home".into()),
                utc_offset_min: Some(60),
                ..SiteSelect::default()
            })
            .unwrap();
        assert_eq!(
            (cur["set_by"].as_str(), cur["pinned"].as_bool()),
            (Some("user"), Some(true))
        );
        let id: SiteId = cur["record"]["id"].as_str().unwrap().parse().unwrap();
        assert_eq!(lock(&s.repo).sites().unwrap().len(), 1, "persisted");
        // Fold 30 h of one quiet channel, then an emitter.
        let subject = BaselineSubject::Cell { index: 1000 };
        let fold = |h: i64, occ: f64| IntervalObservation {
            subject,
            t: Timestamp::from_unix_nanos((7 * 24 + h) * 3_600_000_000_000),
            gain: 0,
            level_db: Some(-100.0 + 20.0 * occ),
            max_db: None,
            occupied_weight_s: occ * 3600.0,
            weight_s: 3600.0,
            observed_s: 3600.0,
            n_eff: 40.0,
            suspect_fraction: 0.0,
            provenance_explained: false,
        };
        for h in 0..30 {
            s.observe(CalKey::Uncalibrated, &fold(h, 0.0)).unwrap();
        }
        let out = s.observe(CalKey::Uncalibrated, &fold(30, 0.8)).unwrap();
        assert_eq!(out.novelty.novelty, 1.0);
        let b = s.baselines_json(Some(id)).unwrap();
        assert_eq!(b["baselines"][0]["subjects"], 1);
        let slots = s
            .slots_json(None, 0.0, 1e9, None, Some(BaselineResolution::AllHours))
            .unwrap();
        assert_eq!(slots["subjects"][0]["f_lo"], 100e6);
        assert!(
            slots["subjects"][0]["reference"]["observed_s"]
                .as_f64()
                .unwrap()
                >= 30.0 * 3600.0
        );
        assert!(s.refreeze(None, None, None).unwrap()["refrozen"].as_u64() == Some(1));
        // Weights: versioned, used by the next scoring pass.
        let (old, new) = s
            .set_weights(
                ScoreWeights {
                    novelty: 4.0,
                    ..ScoreWeights::default()
                },
                "tok",
            )
            .unwrap();
        assert_eq!((old.version, new.version), (1, 2));
        assert_eq!(s.weights_json().unwrap()["history"][0]["version"], 2);
        let input = CandidateInput {
            subject: CandidateSubject::Cells {
                scheme: 1,
                lo_cell: 16_000,
                hi_cell: 16_016,
            },
            freq: FreqRange::new(100e6, 100.1e6),
            snr_db: Some(20.0),
            novelty: out.novelty,
            class_entropy: None,
            decoder_available: false,
            periodicity: None,
            boring: BoringEvidence::default(),
            suspect_fraction: 0.0,
            trust_tested: false,
            expected_interval_s: None,
            min_on_off_s: None,
            next_burst_eta: None,
        };
        let _: NoveltyScore = input.novelty;
        assert_eq!(
            s.score(Timestamp::from_unix_nanos(1), false, &[input]),
            Some(1)
        );
        let c = s.candidates_json(Some(99e6), Some(101e6), 10);
        assert_eq!(
            (c["version"].as_u64(), c["weights"]["version"].as_u64()),
            (Some(1), Some(2))
        );
        assert_eq!(c["candidates"].as_array().unwrap().len(), 1);
        assert_eq!(
            s.candidates_json(Some(200e6), None, 10)["candidates"],
            json!([])
        );
        s.checkpoint(Timestamp::from_unix_nanos(1));
        let _ = std::fs::remove_dir_all(dir);
    }
}
