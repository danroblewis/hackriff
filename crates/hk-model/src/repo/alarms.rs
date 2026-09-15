//! Novelty alarm rows (T-122, ADR-0012 §7, §9; migration 0003).
//!
//! An alarm is an [`Anomaly`] (append-only, with its status history and Explanations) plus one
//! mutable `anomaly_detail` row: the [`AlarmDetail`] evidence and the lifecycle state
//! ([`AlarmState`], [`AlarmLifecycle`]) the engine resumes from. Every lifecycle transition is
//! written together with its `anomaly_status` entry in one transaction:
//!
//! | Transition | Status appended | Note |
//! |---|---|---|
//! | raised | `open` | `raised` |
//! | held | — | — |
//! | cleared | `resolved` | `cleared` |
//! | reopened | `open` | `reopened` (within the cooldown) or `reopened-by-user` |
//! | dismissed | `dismissed` | `dismissed;until_ns=…` |
//! | explained | `open`, then `resolved` | `self-inflicted` |
//!
//! [`Repository::anomalies_page`] lists every anomaly (floor episodes included) with its current
//! status and, for alarms, the detail row.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{RepoError, Repository, blob, bump_extent, enum_text, finite};
use crate::attention::alarm::{AlarmDetail, AlarmKey};
use crate::context::{Anomaly, AnomalyKind, AnomalyStatus, AnomalyStatusChange, AnomalySubject};
use crate::ids::AnomalyId;
use crate::region::{FreqRange, TimeRange};
use crate::time::Timestamp;

/// Most rows one [`Repository::anomalies_page`] returns.
pub const ANOMALY_PAGE_MAX: usize = 1_000;
/// Most rows [`Repository::latest_alarm_rows`] returns.
pub const ALARM_RESUME_MAX: usize = 100_000;

/// Current lifecycle state of an alarm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AlarmState {
    /// Raised or reopened, not cleared.
    Open,
    /// Novelty fell below `off` for the clear count.
    Cleared,
    /// The user dismissed it (until `dismissed_until`).
    Dismissed,
    /// A device provenance step explains the change: a self-inflicted anomaly, not a novelty alarm.
    Explained,
}

/// The last transition applied to an alarm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AlarmLifecycle {
    /// First raise.
    Raised,
    /// Still open (extended).
    Held,
    /// Re-opened within the cooldown or by the user.
    Reopened,
    /// Cleared by hysteresis.
    Cleared,
    /// Dismissed by the user.
    Dismissed,
    /// Dismissal lifted by the user.
    Undismissed,
    /// Written as self-inflicted (ADR-0012 §7.4).
    Explained,
}

/// One alarm's detail row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlarmRow {
    /// The anomaly.
    pub anomaly_id: AnomalyId,
    /// Dedupe key.
    pub key: AlarmKey,
    /// Current state.
    pub state: AlarmState,
    /// Last transition.
    pub last_transition: AlarmLifecycle,
    /// First raise (sample clock).
    pub raised_at: Timestamp,
    /// Last transition or hold (sample clock).
    pub last_t: Timestamp,
    /// Re-opens so far.
    pub reopen_count: u32,
    /// Last clear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleared_at: Option<Timestamp>,
    /// Dismissal expiry (sample clock).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissed_until: Option<Timestamp>,
    /// Frequency hull covered while open (the anomaly row keeps the extent at raise).
    pub freq: FreqRange,
    /// Evidence at the latest raise/reopen/hold.
    pub detail: AlarmDetail,
    /// When the explaining device step happened (sample clock; `explained` rows). The engine
    /// resumes its one-row-per-step dedupe from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explained_step_t: Option<Timestamp>,
}

/// The dedupe key as stored text: `kind=…;site=…;subject=…`.
pub fn alarm_key_text(key: &AlarmKey) -> String {
    use crate::attention::baseline::{BaselineResolution, CalKey, HourOfWeek};
    let full = key.baseline_ref(
        CalKey::Uncalibrated,
        BaselineResolution::AllHours,
        HourOfWeek::try_from(0).expect("slot 0"),
    );
    full.split(';')
        .filter(|p| p.starts_with("kind=") || p.starts_with("site=") || p.starts_with("subject="))
        .collect::<Vec<_>>()
        .join(";")
}

/// Filters for [`Repository::anomalies_page`] (all optional).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AnomalyQuery {
    /// Overlapping this frequency range.
    pub freq: Option<FreqRange>,
    /// Overlapping this time range.
    pub time: Option<TimeRange>,
    /// Of this kind.
    pub kind: Option<AnomalyKind>,
    /// Whose current status is this.
    pub status: Option<AnomalyStatus>,
    /// Rows to skip.
    pub cursor: usize,
    /// Rows to return (capped at [`ANOMALY_PAGE_MAX`]).
    pub limit: usize,
}

/// One listed anomaly.
#[derive(Clone, Debug, PartialEq)]
pub struct AnomalyListing {
    /// The anomaly.
    pub anomaly: Anomaly,
    /// Its current status.
    pub status: AnomalyStatus,
    /// Its alarm detail, for novelty alarms.
    pub alarm: Option<AlarmRow>,
}

/// A page of anomalies, newest first.
#[derive(Clone, Debug, PartialEq)]
pub struct AnomalyPage {
    /// Rows.
    pub rows: Vec<AnomalyListing>,
    /// Cursor of the next page.
    pub next_cursor: Option<usize>,
}

/// Explanations shown per anomaly in lists and stream messages.
pub const TOP_EXPLANATIONS: usize = 3;

/// One anomaly as the control API and the `anomalies` stream show it.
#[derive(Clone, Debug, PartialEq)]
pub struct AnomalyView {
    /// Row, status and alarm detail.
    pub listing: AnomalyListing,
    /// Current explanations, best first.
    pub explanations: Vec<crate::context::Explanation>,
    /// Status history, oldest first (empty in lists and stream messages).
    pub history: Vec<AnomalyStatusChange>,
}

fn secs(t: Timestamp) -> f64 {
    t.as_unix_nanos() as f64 * 1e-9
}

/// The documented JSON of one anomaly (`docs/api.md` "Anomalies and novelty alarms"); `top`
/// limits the explanations and omits `history` (lists, stream messages).
pub fn anomaly_view_json(v: &AnomalyView, top: Option<usize>) -> serde_json::Value {
    use serde_json::json;
    let a = &v.listing.anomaly;
    let alarm = v.listing.alarm.as_ref().map(|r| {
        json!({
            "key": r.key,
            "state": r.state,
            "last_transition": r.last_transition,
            "raised_at": secs(r.raised_at),
            "last_t": secs(r.last_t),
            "reopen_count": r.reopen_count,
            "cleared_at": r.cleared_at.map(secs),
            "dismissed_until": r.dismissed_until.map(secs),
            "f_lo": r.freq.lo_hz,
            "f_hi": r.freq.hi_hz,
            "detail": r.detail,
        })
    });
    let explanations: Vec<_> = v
        .explanations
        .iter()
        .take(top.unwrap_or(usize::MAX))
        .map(|e| {
            json!({
                "id": e.id.to_string(),
                "cause": e.cause,
                "correlation_type": e.correlation_type,
                "score": e.score,
                "provisional": e.provisional,
                "rule_version": e.rule_version,
                "t": secs(e.t),
                "evidence": e.evidence,
            })
        })
        .collect();
    let mut out = json!({
        "id": a.id.to_string(),
        "kind": a.kind,
        "subject": a.subject,
        "f_lo": a.region.freq.lo_hz,
        "f_hi": a.region.freq.hi_hz,
        "t0": secs(a.region.time.start),
        "t1": secs(a.region.time.end),
        "t": secs(a.t),
        "score": a.score,
        "baseline_ref": a.baseline_ref,
        "detector_version": a.detector_version,
        "status": v.listing.status,
        "alarm": alarm,
        "explanations": explanations,
    });
    if top.is_none() {
        out["history"] = json!(
            v.history
                .iter()
                .map(|s| json!({ "status": s.status, "t": secs(s.t), "note": s.note }))
                .collect::<Vec<_>>()
        );
    }
    out
}

fn check_row(row: &AlarmRow) -> Result<(), RepoError> {
    row.detail
        .validate()
        .map_err(|e| RepoError::Invalid(e.to_string()))?;
    if row.detail.key != row.key {
        return Err(RepoError::Invalid(
            "alarm detail key differs from the row key".into(),
        ));
    }
    finite(row.freq.lo_hz, "f_lo")?;
    finite(row.freq.hi_hz, "f_hi")?;
    Ok(())
}

fn insert_status(
    conn: &rusqlite::Connection,
    change: &AnomalyStatusChange,
) -> Result<(), RepoError> {
    conn.execute(
        "INSERT INTO anomaly_status (anomaly_id, status, t, note) VALUES (?1, ?2, ?3, ?4)",
        params![
            blob(change.anomaly_id),
            enum_text(&change.status)?,
            change.t.as_unix_nanos(),
            change.note
        ],
    )?;
    Ok(())
}

impl Repository {
    /// Inserts an alarm: its anomaly, the `statuses` (the first must be `open`), and the detail
    /// row, atomically.
    pub fn insert_alarm(
        &mut self,
        anomaly: &Anomaly,
        row: &AlarmRow,
        statuses: &[AnomalyStatusChange],
    ) -> Result<(), RepoError> {
        check_row(row)?;
        if row.anomaly_id != anomaly.id
            || statuses
                .first()
                .is_none_or(|s| s.status != AnomalyStatus::Open)
            || statuses.iter().any(|s| s.anomaly_id != anomaly.id)
        {
            return Err(RepoError::Invalid(
                "an alarm needs its own anomaly id and an initial open status".into(),
            ));
        }
        let r = &anomaly.region;
        if r.time.end < r.time.start || r.freq.hi_hz < r.freq.lo_hz {
            return Err(RepoError::Invalid("anomaly region is inverted".into()));
        }
        let (subject_kind, subject_id) = match anomaly.subject {
            AnomalySubject::Detection(id) => ("detection", Some(blob(id))),
            AnomalySubject::Emitter(id) => ("emitter", Some(blob(id))),
            AnomalySubject::Region => ("region", None),
        };
        let tx = self.write_tx()?;
        tx.execute(
            "INSERT INTO anomaly (anomaly_id, kind, subject_kind, subject_id, f_lo, f_hi, \
             t_start, t_end, score, t, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                blob(anomaly.id),
                enum_text(&anomaly.kind)?,
                subject_kind,
                subject_id,
                finite(r.freq.lo_hz, "f_lo")?,
                finite(r.freq.hi_hz, "f_hi")?,
                r.time.start.as_unix_nanos(),
                r.time.end.as_unix_nanos(),
                finite(anomaly.score, "score")?,
                anomaly.t.as_unix_nanos(),
                serde_json::to_string(anomaly)?
            ],
        )?;
        bump_extent(&tx, "anomaly", r.freq.width_hz(), r.time.duration_ns())?;
        for s in statuses {
            insert_status(&tx, s)?;
        }
        tx.execute(
            "INSERT INTO anomaly_detail (anomaly_id, alarm_key, kind, site_id, state, \
             last_transition, raised_at, last_t, reopen_count, cleared_at, dismissed_until, f_lo, \
             f_hi, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            detail_params(row)?,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Replaces an alarm's detail row and appends `status` (if any), atomically.
    pub fn update_alarm(
        &mut self,
        row: &AlarmRow,
        status: Option<&AnomalyStatusChange>,
    ) -> Result<(), RepoError> {
        check_row(row)?;
        let tx = self.write_tx()?;
        let p = detail_params(row)?;
        let n = tx.execute(
            "UPDATE anomaly_detail SET alarm_key = ?2, kind = ?3, site_id = ?4, state = ?5, \
             last_transition = ?6, raised_at = ?7, last_t = ?8, reopen_count = ?9, \
             cleared_at = ?10, dismissed_until = ?11, f_lo = ?12, f_hi = ?13, body = ?14 \
             WHERE anomaly_id = ?1",
            p,
        )?;
        if n == 0 {
            return Err(RepoError::NotFound {
                kind: "alarm",
                id: row.anomaly_id.to_string(),
            });
        }
        if let Some(s) = status {
            if s.anomaly_id != row.anomaly_id {
                return Err(RepoError::Invalid("status is for another anomaly".into()));
            }
            insert_status(&tx, s)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// An anomaly's alarm detail, if it is an alarm.
    pub fn alarm_row(&self, id: AnomalyId) -> Result<Option<AlarmRow>, RepoError> {
        let body: Option<String> = self
            .conn
            .prepare_cached("SELECT body FROM anomaly_detail WHERE anomaly_id = ?1")?
            .query_row([blob(id)], |r| r.get(0))
            .optional()?;
        body.map(|b| serde_json::from_str(&b).map_err(RepoError::from))
            .transpose()
    }

    /// The newest alarm row per dedupe key (what the engine resumes from).
    pub fn latest_alarm_rows(&self) -> Result<Vec<AlarmRow>, RepoError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT d.body FROM anomaly_detail d WHERE d.raised_at = \
             (SELECT MAX(raised_at) FROM anomaly_detail e WHERE e.alarm_key = d.alarm_key) \
             ORDER BY d.raised_at DESC LIMIT ?1",
        )?;
        let bodies: Vec<String> = stmt
            .query_map([ALARM_RESUME_MAX as i64], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        bodies
            .iter()
            .map(|b| serde_json::from_str(b).map_err(RepoError::from))
            .collect()
    }

    /// The current status of an anomaly (the last status entry).
    pub fn anomaly_current_status(&self, id: AnomalyId) -> Result<AnomalyStatus, RepoError> {
        let s: Option<String> = self
            .conn
            .prepare_cached(
                "SELECT status FROM anomaly_status WHERE anomaly_id = ?1 \
                 ORDER BY status_id DESC LIMIT 1",
            )?
            .query_row([blob(id)], |r| r.get(0))
            .optional()?;
        let s = s.ok_or_else(|| RepoError::NotFound {
            kind: "anomaly",
            id: id.to_string(),
        })?;
        super::enum_parse(s)
    }

    /// Anomalies of every kind matching `q`, newest first, with current status and alarm detail.
    pub fn anomalies_page(&self, q: &AnomalyQuery) -> Result<AnomalyPage, RepoError> {
        let limit = q.limit.clamp(1, ANOMALY_PAGE_MAX);
        let kind = q.kind.as_ref().map(enum_text).transpose()?;
        let status = q.status.as_ref().map(enum_text).transpose()?;
        let rows: Vec<(String, String, Option<String>)> = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT body, st, detail FROM (SELECT a.body AS body, a.t AS t, \
                 a.anomaly_id AS id, \
                 (SELECT s.status FROM anomaly_status s WHERE s.anomaly_id = a.anomaly_id \
                  ORDER BY s.status_id DESC LIMIT 1) AS st, \
                 (SELECT d.body FROM anomaly_detail d WHERE d.anomaly_id = a.anomaly_id) AS detail \
                 FROM anomaly a \
                 WHERE (?1 IS NULL OR a.f_hi >= ?1) AND (?2 IS NULL OR a.f_lo <= ?2) \
                 AND (?3 IS NULL OR a.t_end >= ?3) AND (?4 IS NULL OR a.t_start <= ?4) \
                 AND (?5 IS NULL OR a.kind = ?5)) \
                 WHERE (?6 IS NULL OR st = ?6) ORDER BY t DESC, id LIMIT ?7 OFFSET ?8",
            )?;
            stmt.query_map(
                params![
                    q.freq.map(|f| f.lo_hz),
                    q.freq.map(|f| f.hi_hz),
                    q.time.map(|t| t.start.as_unix_nanos()),
                    q.time.map(|t| t.end.as_unix_nanos()),
                    kind,
                    status,
                    (limit + 1) as i64,
                    q.cursor as i64,
                ],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?
            .collect::<Result<_, _>>()?
        };
        let more = rows.len() > limit;
        let rows = rows
            .into_iter()
            .take(limit)
            .map(|(body, st, detail)| {
                Ok(AnomalyListing {
                    anomaly: serde_json::from_str(&body)?,
                    status: super::enum_parse(st)?,
                    alarm: detail.map(|d| serde_json::from_str(&d)).transpose()?,
                })
            })
            .collect::<Result<Vec<_>, RepoError>>()?;
        Ok(AnomalyPage {
            rows,
            next_cursor: more.then_some(q.cursor + limit),
        })
    }
}

type DetailParams = (
    [u8; 16],
    String,
    String,
    [u8; 16],
    String,
    String,
    i64,
    i64,
    i64,
    Option<i64>,
    Option<i64>,
    f64,
    f64,
    String,
);

fn detail_params(row: &AlarmRow) -> Result<DetailParams, RepoError> {
    Ok((
        blob(row.anomaly_id),
        alarm_key_text(&row.key),
        row.key.kind.as_str().to_owned(),
        blob(row.key.site),
        enum_text(&row.state)?,
        enum_text(&row.last_transition)?,
        row.raised_at.as_unix_nanos(),
        row.last_t.as_unix_nanos(),
        i64::from(row.reopen_count),
        row.cleared_at.map(Timestamp::as_unix_nanos),
        row.dismissed_until.map(Timestamp::as_unix_nanos),
        row.freq.lo_hz,
        row.freq.hi_hz,
        serde_json::to_string(row)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attention::alarm::{AlarmKind, AlarmSubject, AlarmUnit, ExplanationStage};
    use crate::attention::baseline::{BaselineResolution, CalKey, HourOfWeek};
    use crate::ids::SiteId;
    use crate::region::Region;

    fn t(s: i64) -> Timestamp {
        Timestamp::from_unix_nanos(s * 1_000_000_000)
    }

    fn row(id: AnomalyId, key: AlarmKey, at: i64) -> (Anomaly, AlarmRow) {
        let freq = FreqRange::new(433.0e6, 433.1e6);
        let detail = AlarmDetail {
            schema: crate::attention::ATTENTION_SCHEMA_VERSION,
            key,
            cal: CalKey::Uncalibrated,
            resolution: BaselineResolution::AllHours,
            slot: HourOfWeek::try_from(0).unwrap(),
            unit: AlarmUnit::Fraction,
            observed: 0.8,
            baseline_mean: 0.01,
            baseline_spread: 0.02,
            z: 30.0,
            novelty: 1.0,
            intervals_above: 2,
            observed_s: 900.0,
            stages_applied: ExplanationStage::ORDER.to_vec(),
        };
        let a = Anomaly {
            id,
            kind: key.kind.anomaly_kind(),
            subject: AnomalySubject::Region,
            region: Region::new(freq, TimeRange::new(t(at), t(at + 900))),
            score: 1.0,
            baseline_ref: Some(key.baseline_ref(
                CalKey::Uncalibrated,
                BaselineResolution::AllHours,
                HourOfWeek::try_from(0).unwrap(),
            )),
            t: t(at),
            detector_version: "hk-context.c12-alarm@1".into(),
        };
        let r = AlarmRow {
            anomaly_id: id,
            key,
            state: AlarmState::Open,
            last_transition: AlarmLifecycle::Raised,
            raised_at: t(at),
            last_t: t(at),
            reopen_count: 0,
            cleared_at: None,
            dismissed_until: None,
            freq,
            detail,
            explained_step_t: None,
        };
        (a, r)
    }

    #[test]
    fn alarm_rows_insert_update_list_and_resume() {
        let mut repo = Repository::open_in_memory().unwrap();
        let key = AlarmKey {
            kind: AlarmKind::QuieterThanUsual,
            site: SiteId::new(),
            subject: AlarmSubject::Cells {
                scheme: 1,
                lo_cell: 0,
                hi_cell: 16,
            },
        };
        assert_eq!(
            alarm_key_text(&key),
            format!(
                "kind=quieter-than-usual;site={};subject=cells:1:0..16",
                key.site
            )
        );
        let open = |id, at| AnomalyStatusChange {
            anomaly_id: id,
            status: AnomalyStatus::Open,
            t: t(at),
            note: Some("raised".into()),
        };
        let (a1, mut r1) = row(AnomalyId::new(), key, 100);
        repo.insert_alarm(&a1, &r1, &[open(a1.id, 100)]).unwrap();
        assert!(
            repo.insert_alarm(&a1, &r1, &[]).is_err(),
            "needs an open status"
        );
        r1.state = AlarmState::Cleared;
        r1.last_transition = AlarmLifecycle::Cleared;
        r1.cleared_at = Some(t(4000));
        let clear = AnomalyStatusChange {
            anomaly_id: a1.id,
            status: AnomalyStatus::Resolved,
            t: t(4000),
            note: Some("cleared".into()),
        };
        repo.update_alarm(&r1, Some(&clear)).unwrap();
        assert_eq!(
            repo.anomaly_current_status(a1.id).unwrap(),
            AnomalyStatus::Resolved
        );
        let (a2, r2) = row(AnomalyId::new(), key, 90_000);
        repo.insert_alarm(&a2, &r2, &[open(a2.id, 90_000)]).unwrap();
        assert_eq!(repo.alarm_row(a1.id).unwrap().unwrap(), r1);
        let latest = repo.latest_alarm_rows().unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].anomaly_id, a2.id);

        let page = repo
            .anomalies_page(&AnomalyQuery {
                limit: 1,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].anomaly.id, a2.id, "newest first");
        assert_eq!(page.next_cursor, Some(1));
        let resolved = repo
            .anomalies_page(&AnomalyQuery {
                status: Some(AnomalyStatus::Resolved),
                kind: Some(AnomalyKind::QuieterThanBaseline),
                freq: Some(FreqRange::new(433.05e6, 434e6)),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(resolved.rows.len(), 1);
        assert_eq!(
            resolved.rows[0].alarm.as_ref().unwrap().state,
            AlarmState::Cleared
        );
        assert!(
            repo.anomalies_page(&AnomalyQuery {
                freq: Some(FreqRange::new(1e9, 2e9)),
                limit: 10,
                ..Default::default()
            })
            .unwrap()
            .rows
            .is_empty()
        );
    }
}
