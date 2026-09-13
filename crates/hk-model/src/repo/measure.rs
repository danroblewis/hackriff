//! Measurements and their context: ScanPlan, Survey, CalibrationState, SpurMask, Provenance,
//! Detection, Recording. Measurement rows have insert and read methods only.

use rusqlite::{OptionalExtension, Row, params};

use super::{
    ProvenanceChain, RegionBounds, RepoError, Repository, blob, bodies, body_by_id, bump_extent,
    enum_parse, enum_text, finite, int, opt_blob, region_bounds,
};
use crate::calibration::{CalibrationState, SpurMask};
use crate::detection::{Detection, DetectionFlags};
use crate::ids::{DetectionId, ProvenanceId, ScanPlanId, SurveyId};
use crate::plan::{ScanPlan, Survey, SurveyState, SurveySummary};
use crate::provenance::Provenance;
use crate::recording::{Recording, RecordingTrigger};
use crate::region::{Region, TimeRange};
use crate::time::Timestamp;

const DETECTION_COLUMNS: &str = "detection_id, survey_id, provenance_id, t_start, t_end, \
     f_center, obw, xdb_bw, xdb_level, snr_peak, snr_mean, sk, flags";

/// The detection region query. Parameters: f_center min, f_center max, t_start min, t_end of
/// query, query hi, query lo, query t0.
pub(super) const DETECTION_REGION_SQL: &str = "SELECT detection_id, survey_id, provenance_id, \
     t_start, t_end, f_center, obw, xdb_bw, xdb_level, snr_peak, snr_mean, sk, flags \
     FROM detection \
     WHERE f_center BETWEEN ?1 AND ?2 AND t_start BETWEEN ?3 AND ?4 \
       AND f_lo <= ?5 AND f_hi >= ?6 AND t_end >= ?7 \
     ORDER BY t_start, detection_id";

impl RegionBounds {
    /// Parameters for [`DETECTION_REGION_SQL`]. A matching detection's centre is within half a
    /// max-span of the query band.
    pub(super) fn detection_params(&self) -> (f64, f64, i64, i64, f64, f64, i64) {
        let half = self.f_span / 2.0;
        (
            self.lo - half,
            self.hi + half,
            self.t_start_min,
            self.t1,
            self.hi,
            self.lo,
            self.t0,
        )
    }
}

fn detection_from_row(row: &Row<'_>) -> rusqlite::Result<Detection> {
    Ok(Detection {
        id: DetectionId::from_uuid(uuid::Uuid::from_bytes(row.get(0)?)),
        survey_id: SurveyId::from_uuid(uuid::Uuid::from_bytes(row.get(1)?)),
        provenance_ref: ProvenanceId::from_uuid(uuid::Uuid::from_bytes(row.get(2)?)),
        time: TimeRange::new(
            Timestamp::from_unix_nanos(row.get(3)?),
            Timestamp::from_unix_nanos(row.get(4)?),
        ),
        f_center_hz: row.get(5)?,
        obw_hz: row.get(6)?,
        xdb_bandwidth_hz: row.get(7)?,
        xdb_level_db: row.get(8)?,
        snr_peak_db: row.get(9)?,
        snr_mean_db: row.get(10)?,
        sk: row.get(11)?,
        flags: DetectionFlags::from_bits(row.get(12)?),
    })
}

impl Repository {
    // ---- ScanPlan ----

    /// Inserts a ScanPlan version. `plan.version` must be 1 for a new plan, or the latest
    /// stored version + 1 (editing a plan = inserting its next version).
    pub fn insert_scan_plan(&mut self, plan: &ScanPlan) -> Result<(), RepoError> {
        let tx = self.conn.transaction()?;
        let latest: Option<u32> = tx.query_row(
            "SELECT max(version) FROM scan_plan WHERE plan_id = ?1",
            [blob(plan.id)],
            |r| r.get(0),
        )?;
        let expected = latest.map_or(1, |v| v + 1);
        if plan.version != expected {
            return Err(RepoError::Invalid(format!(
                "scan plan {} version {} must be {expected}",
                plan.id, plan.version
            )));
        }
        tx.execute(
            "INSERT INTO scan_plan (plan_id, version, name, created_at, body) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                blob(plan.id),
                plan.version,
                plan.name,
                plan.created_at.as_unix_nanos(),
                serde_json::to_string(plan)?
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// One ScanPlan version.
    pub fn scan_plan(&self, id: ScanPlanId, version: u32) -> Result<ScanPlan, RepoError> {
        let body: Option<String> = self
            .conn
            .prepare_cached("SELECT body FROM scan_plan WHERE plan_id = ?1 AND version = ?2")?
            .query_row(params![blob(id), version], |r| r.get(0))
            .optional()?;
        match body {
            Some(b) => Ok(serde_json::from_str(&b)?),
            None => Err(RepoError::NotFound {
                kind: "scan plan",
                id: format!("{id} v{version}"),
            }),
        }
    }

    /// The latest version of a ScanPlan.
    pub fn latest_scan_plan(&self, id: ScanPlanId) -> Result<ScanPlan, RepoError> {
        body_by_id(
            &self.conn,
            "SELECT body FROM scan_plan WHERE plan_id = ?1 ORDER BY version DESC LIMIT 1",
            blob(id),
            "scan plan",
        )
    }

    // ---- Survey ----

    /// Inserts a Survey (normally `open`; closed surveys may be imported).
    pub fn insert_survey(&mut self, survey: &Survey) -> Result<(), RepoError> {
        self.conn.execute(
            "INSERT INTO survey (survey_id, plan_id, plan_version, device_id, state, t_start, \
             t_end, summary) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                blob(survey.id),
                blob(survey.plan_id),
                survey.plan_version,
                survey.device_id,
                enum_text(&survey.state)?,
                survey.t_start.as_unix_nanos(),
                survey.t_end.map(Timestamp::as_unix_nanos),
                survey
                    .summary
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?
            ],
        )?;
        Ok(())
    }

    /// Ends an open Survey: `open → closed` or `open → aborted`.
    pub fn finish_survey(
        &mut self,
        id: SurveyId,
        state: SurveyState,
        t_end: Timestamp,
        summary: &SurveySummary,
    ) -> Result<(), RepoError> {
        if state == SurveyState::Open {
            return Err(RepoError::Invalid(
                "a survey can only finish as closed or aborted".into(),
            ));
        }
        let changed = self.conn.execute(
            "UPDATE survey SET state = ?1, t_end = ?2, summary = ?3 \
             WHERE survey_id = ?4 AND state = 'open'",
            params![
                enum_text(&state)?,
                t_end.as_unix_nanos(),
                serde_json::to_string(summary)?,
                blob(id)
            ],
        )?;
        if changed == 0 {
            let _ = self.survey(id)?; // NotFound if absent
            return Err(RepoError::Invalid(format!("survey {id} is not open")));
        }
        Ok(())
    }

    /// One Survey.
    pub fn survey(&self, id: SurveyId) -> Result<Survey, RepoError> {
        type Raw = (
            [u8; 16],
            u32,
            String,
            String,
            i64,
            Option<i64>,
            Option<String>,
        );
        let raw: Option<Raw> = self
            .conn
            .prepare_cached(
                "SELECT plan_id, plan_version, device_id, state, t_start, t_end, summary \
                 FROM survey WHERE survey_id = ?1",
            )?
            .query_row([blob(id)], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })
            .optional()?;
        let Some((plan_id, plan_version, device_id, state, t_start, t_end, summary)) = raw else {
            return Err(RepoError::NotFound {
                kind: "survey",
                id: id.to_string(),
            });
        };
        Ok(Survey {
            id,
            plan_id: ScanPlanId::from_uuid(uuid::Uuid::from_bytes(plan_id)),
            plan_version,
            device_id,
            state: enum_parse(state)?,
            t_start: Timestamp::from_unix_nanos(t_start),
            t_end: t_end.map(Timestamp::from_unix_nanos),
            summary: summary.map(|s| serde_json::from_str(&s)).transpose()?,
        })
    }

    // ---- CalibrationState / SpurMask ----

    /// Inserts a CalibrationState version.
    pub fn insert_calibration_state(&mut self, cal: &CalibrationState) -> Result<(), RepoError> {
        self.conn.execute(
            "INSERT INTO calibration_state (cal_id, supersedes, device_id, measured_at, body) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                blob(cal.id),
                opt_blob(cal.supersedes),
                cal.device_id,
                cal.measured_at.as_unix_nanos(),
                serde_json::to_string(cal)?
            ],
        )?;
        Ok(())
    }

    /// One CalibrationState version.
    pub fn calibration_state(
        &self,
        id: crate::ids::CalibrationStateId,
    ) -> Result<CalibrationState, RepoError> {
        body_by_id(
            &self.conn,
            "SELECT body FROM calibration_state WHERE cal_id = ?1",
            blob(id),
            "calibration state",
        )
    }

    /// Inserts a SpurMask version.
    pub fn insert_spur_mask(&mut self, mask: &SpurMask) -> Result<(), RepoError> {
        self.conn.execute(
            "INSERT INTO spur_mask (spur_id, supersedes, device_id, measured_at, body) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                blob(mask.id),
                opt_blob(mask.supersedes),
                mask.device_id,
                mask.measured_at.as_unix_nanos(),
                serde_json::to_string(mask)?
            ],
        )?;
        Ok(())
    }

    /// One SpurMask version.
    pub fn spur_mask(&self, id: crate::ids::SpurMaskId) -> Result<SpurMask, RepoError> {
        body_by_id(
            &self.conn,
            "SELECT body FROM spur_mask WHERE spur_id = ?1",
            blob(id),
            "spur mask",
        )
    }

    // ---- Provenance ----

    /// Stores a Provenance value, or finds the identical one already stored, and returns its
    /// id. Identical values always map to the same row. The calibration and spur-mask versions
    /// it names must already be stored.
    pub fn intern_provenance(&mut self, p: &Provenance) -> Result<ProvenanceId, RepoError> {
        let canonical = serde_json::to_string(p)?;
        // Round-trip check: a value that does not read back (e.g. NaN stored as null) must not
        // become a trust record.
        if serde_json::from_str::<Provenance>(&canonical)? != *p {
            return Err(RepoError::Invalid(
                "provenance does not round-trip (non-finite float?)".into(),
            ));
        }
        let lookup = "SELECT provenance_id FROM provenance WHERE canonical = ?1";
        if let Some(id) = self
            .conn
            .prepare_cached(lookup)?
            .query_row([&canonical], |r| r.get::<_, [u8; 16]>(0))
            .optional()?
        {
            return Ok(ProvenanceId::from_uuid(uuid::Uuid::from_bytes(id)));
        }
        let id = ProvenanceId::new();
        self.conn
            .prepare_cached(
                "INSERT INTO provenance (provenance_id, canonical, device_id, overload, \
                 clip_count, cal_id, spur_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![
                blob(id),
                canonical,
                p.device_id,
                p.overload,
                int(p.clip_count, "clip_count")?,
                opt_blob(p.calibration_state_ref),
                opt_blob(p.spur_mask_ref)
            ])?;
        Ok(id)
    }

    /// One Provenance value.
    pub fn provenance(&self, id: ProvenanceId) -> Result<Provenance, RepoError> {
        body_by_id(
            &self.conn,
            "SELECT canonical FROM provenance WHERE provenance_id = ?1",
            blob(id),
            "provenance",
        )
    }

    /// Resolves Provenance → CalibrationState / SpurMask.
    pub fn provenance_chain(&self, id: ProvenanceId) -> Result<ProvenanceChain, RepoError> {
        let provenance = self.provenance(id)?;
        let calibration = provenance
            .calibration_state_ref
            .map(|c| self.calibration_state(c))
            .transpose()?;
        let spur_mask = provenance
            .spur_mask_ref
            .map(|s| self.spur_mask(s))
            .transpose()?;
        Ok(ProvenanceChain {
            id,
            provenance,
            calibration,
            spur_mask,
        })
    }

    // ---- Detection ----

    /// Inserts one detection. Prefer [`Self::insert_detections`] for streams.
    pub fn insert_detection(&mut self, detection: &Detection) -> Result<(), RepoError> {
        self.insert_detections(std::slice::from_ref(detection))
    }

    /// Inserts a batch of detections in **one transaction**: all or none are written.
    pub fn insert_detections(&mut self, detections: &[Detection]) -> Result<(), RepoError> {
        if detections.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        let (mut max_f_span, mut max_t_span) = (0.0_f64, 0_i64);
        {
            let mut stmt = tx.prepare_cached(&format!(
                "INSERT INTO detection ({DETECTION_COLUMNS}, f_lo, f_hi) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)"
            ))?;
            for d in detections {
                finite(d.f_center_hz, "f_center_hz")?;
                finite(d.obw_hz, "obw_hz")?;
                let freq = d.freq();
                max_f_span = max_f_span.max(freq.width_hz());
                max_t_span = max_t_span.max(d.time.duration_ns());
                stmt.execute(params![
                    blob(d.id),
                    blob(d.survey_id),
                    blob(d.provenance_ref),
                    d.time.start.as_unix_nanos(),
                    d.time.end.as_unix_nanos(),
                    d.f_center_hz,
                    d.obw_hz,
                    d.xdb_bandwidth_hz,
                    d.xdb_level_db,
                    d.snr_peak_db,
                    d.snr_mean_db,
                    d.sk,
                    d.flags.bits(),
                    freq.lo_hz,
                    freq.hi_hz
                ])?;
            }
        }
        bump_extent(&tx, "detection", max_f_span, max_t_span)?;
        tx.commit()?;
        Ok(())
    }

    /// One detection.
    pub fn detection(&self, id: DetectionId) -> Result<Detection, RepoError> {
        self.conn
            .prepare_cached(&format!(
                "SELECT {DETECTION_COLUMNS} FROM detection WHERE detection_id = ?1"
            ))?
            .query_row([blob(id)], detection_from_row)
            .optional()?
            .ok_or_else(|| RepoError::NotFound {
                kind: "detection",
                id: id.to_string(),
            })
    }

    /// Detections whose frequency × time box overlaps `region` (closed intervals), ordered by
    /// start time. The region-over-time query (docs/07 §4 step 2; AWARE-042).
    pub fn detections_in_region(&self, region: &Region) -> Result<Vec<Detection>, RepoError> {
        let p = region_bounds(&self.conn, "detection", region)?.detection_params();
        let mut stmt = self.conn.prepare_cached(DETECTION_REGION_SQL)?;
        let rows = stmt
            .query_map(
                params![p.0, p.1, p.2, p.3, p.4, p.5, p.6],
                detection_from_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Number of stored detections.
    pub fn detection_count(&self) -> Result<u64, RepoError> {
        let n: i64 = self
            .conn
            .query_row("SELECT count(*) FROM detection", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    // ---- Recording ----

    /// Inserts a Recording row (the SigMF files are written by C25 first).
    pub fn insert_recording(&mut self, rec: &Recording) -> Result<(), RepoError> {
        let (trigger_kind, det, demod) = match rec.trigger {
            RecordingTrigger::Detection(d) => ("detection", Some(blob(d)), None),
            RecordingTrigger::Demodulation(m) => ("demodulation", None, Some(blob(m))),
            RecordingTrigger::Scheduler => ("scheduler", None, None),
            RecordingTrigger::Manual => ("manual", None, None),
        };
        self.conn.execute(
            "INSERT INTO recording (recording_id, kind, t_start, t_end, f_center, trigger_kind, \
             trigger_detection_id, trigger_demodulation_id, size_bytes, retention_class, \
             content_class, provenance_id, body) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                blob(rec.id),
                enum_text(&rec.kind)?,
                rec.time.start.as_unix_nanos(),
                rec.time.end.as_unix_nanos(),
                rec.f_center_hz,
                trigger_kind,
                det,
                demod,
                int(rec.size_bytes, "size_bytes")?,
                enum_text(&rec.retention_class)?,
                enum_text(&rec.content_class)?,
                blob(rec.provenance_ref),
                serde_json::to_string(rec)?
            ],
        )?;
        Ok(())
    }

    /// One Recording.
    pub fn recording(&self, id: crate::ids::RecordingId) -> Result<Recording, RepoError> {
        body_by_id(
            &self.conn,
            "SELECT body FROM recording WHERE recording_id = ?1",
            blob(id),
            "recording",
        )
    }

    /// Recordings triggered by a detection (its IQ snippets).
    pub fn recordings_for_detection(&self, id: DetectionId) -> Result<Vec<Recording>, RepoError> {
        bodies(
            &self.conn,
            "SELECT body FROM recording WHERE trigger_detection_id = ?1 ORDER BY t_start",
            [blob(id)],
        )
    }
}
