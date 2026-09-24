//! Measurements and their context: ScanPlan, Survey, CalibrationState, SpurMask, Provenance,
//! Detection, Recording. Measurement rows have insert and read methods only.

use std::collections::HashMap;

use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::{
    ProvenanceChain, RegionBounds, RepoError, Repository, blob, bodies, body_by_id, bump_extent,
    canonical_with_hash, enum_parse, enum_text, finite, gate, int, opt_blob, region_bounds,
};
use crate::calibration::{CalibrationState, SpurMask};
use crate::detection::{Detection, DetectionFlags, SpurReason};
use crate::ids::{DetectionId, ProvenanceId, ScanPlanId, SpurMaskId, SurveyId};
use crate::plan::{ScanPlan, Survey, SurveyState, SurveySummary};
use crate::provenance::Provenance;
use crate::recording::{Recording, RecordingKind, RecordingTrigger};
use crate::region::{Region, TimeRange};
use crate::time::Timestamp;

macro_rules! detection_columns {
    () => {
        "detection_id, survey_id, provenance_id, t_start, t_end, f_center, obw, xdb_bw, \
         xdb_level, snr_peak, snr_mean, sk, flags, peak_dbfs, peak_dbm, clip_count, \
         detector_version, spur_reason, spur_mask_id"
    };
}

/// The detection region query. Parameters: f_center min, f_center max, t_start min, t_end of
/// query, query hi, query lo, query t0.
pub(super) const DETECTION_REGION_SQL: &str = concat!(
    "SELECT ",
    detection_columns!(),
    " FROM detection \
     WHERE f_center BETWEEN ?1 AND ?2 AND t_start BETWEEN ?3 AND ?4 \
       AND f_lo <= ?5 AND f_hi >= ?6 AND t_end >= ?7 \
     ORDER BY t_start, detection_id"
);

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
    let mut flags = DetectionFlags::from_bits(row.get(12)?);
    let reason: Option<String> = row.get(17)?;
    let mask: Option<[u8; 16]> = row.get(18)?;
    flags.spur_reason = match reason {
        None => None,
        Some(kind) => {
            let mask = mask.map(|m| SpurMaskId::from_uuid(uuid::Uuid::from_bytes(m)));
            Some(SpurReason::from_parts(&kind, mask).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    17,
                    Type::Text,
                    format!("invalid spur reason {kind:?}").into(),
                )
            })?)
        }
    };
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
        flags,
        peak_level_dbfs: row.get(13)?,
        peak_level_dbm: row.get(14)?,
        clip_count: row.get(15)?,
        detector_version: row.get(16)?,
    })
}

impl Repository {
    // ---- ScanPlan ----

    /// Inserts a ScanPlan version. `plan.version` must be 1 for a new plan, or the latest
    /// stored version + 1 (editing a plan = inserting its next version).
    pub fn insert_scan_plan(&mut self, plan: &ScanPlan) -> Result<(), RepoError> {
        let tx = self.write_tx()?;
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

    /// The most recently measured [`CalibrationState`] for `device_id`, or `None` when the device
    /// has never been calibrated (T-560).
    ///
    /// A caller with a `device_id` (e.g. a raster fit or an API route) reads this instead of
    /// re-fitting from raw IQ: the estimate is recorded once, under C05, with its own provenance
    /// (`method`, `measured_at`), and every later query reads the stored row rather than paying
    /// for the fit again. `method` narrows to one measurement technique, so a raster-derived ppm
    /// and an FM-pilot-derived one never shadow each other.
    pub fn latest_calibration_state_for_device(
        &self,
        device_id: &str,
        method: &crate::calibration::CalibrationMethod,
    ) -> Result<Option<CalibrationState>, RepoError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT body FROM calibration_state WHERE device_id = ?1 \
             ORDER BY measured_at DESC, rowid DESC",
        )?;
        let mut rows = stmt.query(params![device_id])?;
        while let Some(row) = rows.next()? {
            let body: String = row.get(0)?;
            let cal: CalibrationState = serde_json::from_str(&body)?;
            if &cal.method == method {
                return Ok(Some(cal));
            }
        }
        Ok(None)
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
    pub fn spur_mask(&self, id: SpurMaskId) -> Result<SpurMask, RepoError> {
        body_by_id(
            &self.conn,
            "SELECT body FROM spur_mask WHERE spur_id = ?1",
            blob(id),
            "spur mask",
        )
    }

    // ---- Provenance ----

    /// Stores a Provenance value, or finds the identical one already stored, and returns its
    /// id. Identical values (after canonicalisation: key order, `-0.0`) always map to the same
    /// row, also across concurrent connections. The calibration and spur-mask versions it names
    /// must already be stored.
    pub fn intern_provenance(&mut self, p: &Provenance) -> Result<ProvenanceId, RepoError> {
        let (canonical, hash) = canonical_with_hash(p, "provenance")?;
        let tx = self.write_tx()?;
        tx.prepare_cached(
            "INSERT INTO provenance (provenance_id, content_hash, canonical, device_id, overload, \
             quantisation_limited, cal_id, spur_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT (content_hash) DO NOTHING",
        )?
        .execute(params![
            blob(ProvenanceId::new()),
            hash.as_bytes(),
            canonical,
            p.device_id,
            p.overload,
            p.quantisation_limited,
            opt_blob(p.calibration_state_ref),
            opt_blob(p.spur_mask_ref)
        ])?;
        let (id, stored): ([u8; 16], String) = tx
            .prepare_cached(
                "SELECT provenance_id, canonical FROM provenance WHERE content_hash = ?1",
            )?
            .query_row([hash.as_bytes()], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if stored != canonical {
            return Err(RepoError::Invalid(format!(
                "provenance content hash collision on {hash}"
            )));
        }
        tx.commit()?;
        Ok(ProvenanceId::from_uuid(uuid::Uuid::from_bytes(id)))
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
    ///
    /// Refuses (whole batch) a detection with `clip_count > 0` or an overloaded provenance but no
    /// `flags.clipped` ([`RepoError::UnflaggedClipping`]), or with inconsistent dependent flags.
    pub fn insert_detections(&mut self, detections: &[Detection]) -> Result<(), RepoError> {
        if detections.is_empty() {
            return Ok(());
        }
        let tx = self.write_tx()?;
        insert_detections_on(&tx, detections)?;
        tx.commit()?;
        Ok(())
    }

    /// One detection.
    ///
    /// **T-598:** the measured columns come from the (immutable) row; the suspect flags a standing
    /// cross-centre retune verdict implies are applied over them
    /// ([`super::retune::apply_verdicts`]), so a reader sees what is known about the line now
    /// without the measurement ever having been rewritten. Revoking the verdict takes them away
    /// again by construction.
    pub fn detection(&self, id: DetectionId) -> Result<Detection, RepoError> {
        let mut found = self
            .conn
            .prepare_cached(concat!(
                "SELECT ",
                detection_columns!(),
                " FROM detection WHERE detection_id = ?1"
            ))?
            .query_row([blob(id)], detection_from_row)
            .optional()?
            .map(|d| [d])
            .ok_or_else(|| RepoError::NotFound {
                kind: "detection",
                id: id.to_string(),
            })?;
        super::retune::apply_verdicts(&self.conn, &mut found)?;
        let [d] = found;
        Ok(d)
    }

    /// Detections whose frequency × time box overlaps `region` (closed intervals), ordered by
    /// start time. The region-over-time query (docs/07 §4 step 2; AWARE-042).
    pub fn detections_in_region(&self, region: &Region) -> Result<Vec<Detection>, RepoError> {
        let tx = self.read_tx()?;
        let p = region_bounds(&tx, "detection", region)?.detection_params();
        let mut stmt = tx.prepare_cached(DETECTION_REGION_SQL)?;
        let mut rows = stmt
            .query_map(
                params![p.0, p.1, p.2, p.3, p.4, p.5, p.6],
                detection_from_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        // T-598: the standing retune verdicts over these rows (see [`Self::detection`]).
        super::retune::apply_verdicts(&tx, &mut rows)?;
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

    /// Inserts a Recording row (the SigMF files are written by C25 first). IQ and audio are
    /// content: a class that does not permit content is refused ([`RepoError::GatedContent`]).
    pub fn insert_recording(&mut self, rec: &Recording) -> Result<(), RepoError> {
        gate("recording", rec.content_class, true)?;
        let (trigger_kind, det, demod) = match rec.trigger {
            RecordingTrigger::Detection(d) => ("detection", Some(blob(d)), None),
            RecordingTrigger::Demodulation(m) => ("demodulation", None, Some(blob(m))),
            RecordingTrigger::Scheduler => ("scheduler", None, None),
            RecordingTrigger::Manual => ("manual", None, None),
            RecordingTrigger::Analyze => ("analyze", None, None),
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

    /// Recordings whose samples overlap `[t0_ns, t1_ns)` (either bound open when `None`), of
    /// `kind` when given, **newest first**, at most `limit`; with the total that matched, so a
    /// caller can say how many it omitted rather than implying the page is everything (T-469).
    ///
    /// A plain query over the existing `idx_recording_t_start` index — **no second index is
    /// maintained**: what is on disk is whatever rows are here, and a catalogue kept beside them
    /// could only go stale. Half-open in both directions, like every other window in the model:
    /// a recording ending exactly at `t0` does not overlap.
    pub fn recordings_in(
        &self,
        t0_ns: Option<i64>,
        t1_ns: Option<i64>,
        kind: Option<RecordingKind>,
        limit: usize,
    ) -> Result<(Vec<Recording>, u64), RepoError> {
        const WHERE: &str = "WHERE t_end > ?1 AND t_start < ?2 AND (?3 IS NULL OR kind = ?3)";
        let (t0, t1) = (t0_ns.unwrap_or(i64::MIN), t1_ns.unwrap_or(i64::MAX));
        let kind = kind.map(|k| enum_text(&k)).transpose()?;
        let matched: i64 = self
            .conn
            .prepare_cached(&format!("SELECT count(*) FROM recording {WHERE}"))?
            .query_row(params![t0, t1, kind], |r| r.get(0))?;
        // Newest first, ties broken by id so a page is stable across calls.
        let rows = bodies(
            &self.conn,
            &format!(
                "SELECT body FROM recording {WHERE} ORDER BY t_start DESC, recording_id DESC \
                 LIMIT ?4"
            ),
            params![t0, t1, kind, int(limit as u64, "limit")?],
        )?;
        Ok((rows, matched.max(0) as u64))
    }
}

/// [`Repository::insert_detections`] inside an open write transaction.
pub(super) fn insert_detections_on(
    conn: &Connection,
    detections: &[Detection],
) -> Result<(), RepoError> {
    let (mut max_f_span, mut max_t_span) = (0.0_f64, 0_i64);
    {
        let mut overloaded: HashMap<ProvenanceId, bool> = HashMap::new();
        let mut lookup =
            conn.prepare_cached("SELECT overload FROM provenance WHERE provenance_id = ?1")?;
        let mut stmt = conn.prepare_cached(concat!(
            "INSERT INTO detection (",
            detection_columns!(),
            ", f_lo, f_hi) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, \
             ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)"
        ))?;
        for d in detections {
            finite(d.f_center_hz, "f_center_hz")?;
            finite(d.obw_hz, "obw_hz")?;
            finite(f64::from(d.peak_level_dbfs), "peak_level_dbfs")?;
            if let Some(rule) = d.flags.inconsistency() {
                return Err(RepoError::Invalid(format!("detection {}: {rule}", d.id)));
            }
            let overload = match overloaded.get(&d.provenance_ref) {
                Some(o) => *o,
                None => {
                    // A missing provenance fails on the foreign key below.
                    let o = lookup
                        .query_row([blob(d.provenance_ref)], |r| r.get::<_, bool>(0))
                        .optional()?
                        .unwrap_or(false);
                    overloaded.insert(d.provenance_ref, o);
                    o
                }
            };
            if (overload || d.clip_count > 0) && !d.flags.clipped {
                return Err(RepoError::UnflaggedClipping { detection: d.id });
            }
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
                d.peak_level_dbfs,
                d.peak_level_dbm,
                d.clip_count,
                d.detector_version,
                d.flags.spur_reason.map(|r| r.kind_str()),
                opt_blob(d.flags.spur_reason.and_then(|r| r.mask())),
                freq.lo_hz,
                freq.hi_hz
            ])?;
        }
    }
    bump_extent(conn, "detection", max_f_span, max_t_span)?;
    Ok(())
}
