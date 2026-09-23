//! Saved measurements (T-818 / MAP-18, docs/25 §4 and §10, ADR-0023): a measurement a researcher
//! takes on the canvas — Δf, Δt, bandwidth, duration, symbol rate or period — kept as a durable
//! object with **value + unit + place (a time–frequency extent) + time + provenance**, instead of
//! a readout that vanishes on mouse-up.
//!
//! - **The value is computed here, never accepted.** A writer supplies the `kind`, the two
//!   `cursors` (the place) and, for a rate or period, the cycle count `n`;
//!   [`compute_measurement`] derives `value`, `unit` and the place, and
//!   [`Measurement::validate`] refuses a record whose stored value is not exactly that
//!   computation (docs/25 §10.4). The client's live drag readout is presentation; the stored
//!   number has one authority.
//! - **`basis` says what the value is a function of.** Today every kind is
//!   [`MeasurementBasis::Cursors`]: the span the two cursors mark (for `bandwidth`, the
//!   user-marked width docs/25 §4 allows; for `symbol_rate`/`period`, `n` cycles over the marked
//!   time span, inspectrum-style). A data-derived −3 dB width or an estimated symbol rate over
//!   the IQ under the place is a different basis and would say so, never silently replace this.
//! - **User metadata, never detection input** (docs/25 §10.7): nothing in the pipeline reads this
//!   table. It lives in the run's user-metadata database beside bookmarks and selections.
//! - **Capture clock vs wall clock.** Cursor times, `t0`/`t1` and the provenance `t_capture` are
//!   capture-clock ("when the air was"); `authored_at`, `created_at`, `updated_at` are wall-clock
//!   audit times ("when the human acted"). Stored apart, never compared.
//! - **Paged.** [`Repository::measurements_in`] is `limit`-bounded ([`MEASUREMENT_PAGE_MAX`]) with
//!   an optional window and collection filter; "durable" is not "unbounded" (docs/25 §10.3).
//! - The table is ensured on first use (the selections pattern), so no schema migration is needed
//!   and the MAP-16..19 stores can land in any order.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{RepoError, Repository, blob};
use crate::ids::MeasurementId;
use crate::time::Timestamp;

/// Longest note, characters.
pub const MEASUREMENT_NOTE_MAX: usize = 4000;
/// Longest `device_id` / actor reference, characters.
pub const MEASUREMENT_REF_MAX: usize = 128;
/// Most rows one [`Repository::measurements_in`] page returns.
pub const MEASUREMENT_PAGE_MAX: usize = 2000;
/// Largest cycle count `n` for a symbol-rate/period measurement.
pub const MEASUREMENT_N_MAX: u32 = 1_000_000;

const ENSURE_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS saved_measurement (
    measurement_id BLOB    PRIMARY KEY CHECK (length(measurement_id) = 16),
    collection_id  TEXT,
    f_lo           REAL    NOT NULL CHECK (f_lo >= 0),
    f_hi           REAL    NOT NULL CHECK (f_hi >= f_lo),
    t0             INTEGER NOT NULL,
    t1             INTEGER NOT NULL CHECK (t1 >= t0),
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    body           TEXT    NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_saved_measurement_t ON saved_measurement (t0, t1);
CREATE INDEX IF NOT EXISTS idx_saved_measurement_collection ON saved_measurement (collection_id);";

/// What quantity a measurement is (docs/25 §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeasurementKind {
    /// A frequency span between two cursors (Hz).
    DeltaF,
    /// A time span between two cursors (s).
    DeltaT,
    /// The marked width of a region (Hz); must be positive.
    Bandwidth,
    /// The time extent of a burst or region (s); must be positive.
    Duration,
    /// `n` cycles of a repeating feature over the marked time span: `n / Δt` (Bd).
    SymbolRate,
    /// The reciprocal of [`Self::SymbolRate`]: `Δt / n` (s).
    Period,
}

impl MeasurementKind {
    /// The wire form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeltaF => "delta_f",
            Self::DeltaT => "delta_t",
            Self::Bandwidth => "bandwidth",
            Self::Duration => "duration",
            Self::SymbolRate => "symbol_rate",
            Self::Period => "period",
        }
    }

    /// Whether the kind needs a cycle count `n`.
    pub fn needs_cycles(self) -> bool {
        matches!(self, Self::SymbolRate | Self::Period)
    }
}

/// What the stored value is a function of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeasurementBasis {
    /// The span the two cursors mark (and `n`, for a rate or period).
    Cursors,
}

impl MeasurementBasis {
    /// The wire form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cursors => "cursors",
        }
    }
}

/// The honesty tier the measurement was taken over (docs/14 T-341, docs/16 §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MeasurementTier {
    /// Live-IQ detail.
    LiveIq,
    /// Spectrum history (the tile pyramid).
    SpectrumHistory,
    /// Survey overview (reduced, non-live-IQ data).
    SurveyOverview,
}

impl MeasurementTier {
    /// The wire form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LiveIq => "live-iq",
            Self::SpectrumHistory => "spectrum-history",
            Self::SurveyOverview => "survey-overview",
        }
    }
}

/// One cursor: a point on the (time × frequency) plane.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasurementCursor {
    /// Frequency, Hz.
    pub f_hz: f64,
    /// Capture-clock time.
    #[serde(rename = "t_ns")]
    pub t: Timestamp,
}

/// The docs/25 §2 provenance stamp, written by the **server**: the view context the client
/// reported plus what the server authoritatively knows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasurementProvenance {
    /// Whose coverage the measurement rests on (the pane's device), or `None`.
    pub device_id: Option<String>,
    /// The view's centre frequency when measured, Hz.
    pub center_hz: f64,
    /// The view's frequency span when measured, Hz.
    pub span_hz: f64,
    /// The named device's sample rate at measuring time, when the server holds that device.
    pub sample_rate_hz: Option<f64>,
    /// Capture-clock window the view was showing, `[start, end]`.
    #[serde(rename = "t_capture_ns")]
    pub t_capture: [Timestamp; 2],
    /// Honesty tier the view was drawn at.
    pub tier: MeasurementTier,
    /// Wall-clock instant of authoring (audit, never a measurement time).
    #[serde(rename = "authored_at_ns")]
    pub authored_at: Timestamp,
    /// Token fingerprint of who took it (never the token).
    pub actor: Option<String>,
    /// Always `true`: a human-authored object.
    pub authored: bool,
}

/// What [`compute_measurement`] derives from `(kind, cursors, n)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Computed {
    /// The measured value.
    pub value: f64,
    /// Its unit (`Hz`, `s` or `Bd`).
    pub unit: &'static str,
    /// Lower frequency edge of the place, Hz.
    pub f_lo_hz: f64,
    /// Upper frequency edge of the place, Hz.
    pub f_hi_hz: f64,
    /// Capture-clock start of the place.
    pub t0: Timestamp,
    /// Capture-clock end of the place.
    pub t1: Timestamp,
    /// What the value is a function of.
    pub basis: MeasurementBasis,
}

fn invalid(msg: impl Into<String>) -> RepoError {
    RepoError::Invalid(msg.into())
}

/// The one computation of a measurement's value, unit and place from its cursors (docs/25 §10.4).
/// Needs exactly two cursors with finite, non-negative frequencies; `n` (1..=[`MEASUREMENT_N_MAX`])
/// exactly when the kind is a rate or period.
pub fn compute_measurement(
    kind: MeasurementKind,
    cursors: &[MeasurementCursor],
    n: Option<u32>,
) -> Result<Computed, RepoError> {
    let [a, b] = cursors else {
        return Err(invalid(format!(
            "a measurement needs exactly 2 cursors, got {}",
            cursors.len()
        )));
    };
    for c in [a, b] {
        if !(c.f_hz.is_finite() && c.f_hz >= 0.0) {
            return Err(invalid("cursor f_hz must be a finite, non-negative Hz"));
        }
    }
    match (kind.needs_cycles(), n) {
        (true, None) => {
            return Err(invalid(format!(
                "a {} measurement needs n, the cycle count the cursors span",
                kind.as_str()
            )));
        }
        (true, Some(n)) if !(1..=MEASUREMENT_N_MAX).contains(&n) => {
            return Err(invalid(format!("n must be in 1..={MEASUREMENT_N_MAX}")));
        }
        (false, Some(_)) => {
            return Err(invalid(format!(
                "n applies only to symbol_rate and period, not {}",
                kind.as_str()
            )));
        }
        _ => {}
    }
    let (f_lo_hz, f_hi_hz) = (a.f_hz.min(b.f_hz), a.f_hz.max(b.f_hz));
    let (t0, t1) = (a.t.min(b.t), a.t.max(b.t));
    let df = f_hi_hz - f_lo_hz;
    let dt = (t1.as_unix_nanos() - t0.as_unix_nanos()) as f64 / 1e9;
    let (value, unit) = match kind {
        MeasurementKind::DeltaF => (df, "Hz"),
        MeasurementKind::DeltaT => (dt, "s"),
        MeasurementKind::Bandwidth | MeasurementKind::Duration => {
            let (v, unit, axis) = if kind == MeasurementKind::Bandwidth {
                (df, "Hz", "frequency")
            } else {
                (dt, "s", "time")
            };
            if v <= 0.0 {
                return Err(invalid(format!(
                    "a {} needs the cursors apart in {axis}",
                    kind.as_str()
                )));
            }
            (v, unit)
        }
        MeasurementKind::SymbolRate | MeasurementKind::Period => {
            if dt <= 0.0 {
                return Err(invalid(format!(
                    "a {} needs the cursors apart in time",
                    kind.as_str()
                )));
            }
            let n = f64::from(n.unwrap_or(1));
            if kind == MeasurementKind::SymbolRate {
                (n / dt, "Bd")
            } else {
                (dt / n, "s")
            }
        }
    };
    Ok(Computed {
        value,
        unit,
        f_lo_hz,
        f_hi_hz,
        t0,
        t1,
        basis: MeasurementBasis::Cursors,
    })
}

/// A durable, human-taken measurement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measurement {
    /// Id.
    pub id: MeasurementId,
    /// The marker collection (MAP-17) it belongs to, as a UUID string; `None` = loose.
    pub collection_id: Option<String>,
    /// What quantity it is.
    pub kind: MeasurementKind,
    /// The value, computed by [`compute_measurement`].
    pub value: f64,
    /// Its unit.
    pub unit: String,
    /// What the value is a function of.
    pub basis: MeasurementBasis,
    /// Lower frequency edge of the place, Hz.
    pub f_lo_hz: f64,
    /// Upper frequency edge of the place, Hz.
    pub f_hi_hz: f64,
    /// Capture-clock start of the place.
    #[serde(rename = "t0_ns")]
    pub t0: Timestamp,
    /// Capture-clock end of the place.
    #[serde(rename = "t1_ns")]
    pub t1: Timestamp,
    /// The raw cursors that produced the value.
    pub cursors: Vec<MeasurementCursor>,
    /// Cycle count for a rate or period.
    pub n: Option<u32>,
    /// Optional note.
    pub note: Option<String>,
    /// The docs/25 §2 stamp.
    pub provenance: MeasurementProvenance,
    /// Wall-clock creation.
    #[serde(rename = "created_at_ns")]
    pub created_at: Timestamp,
    /// Wall-clock last change.
    #[serde(rename = "updated_at_ns")]
    pub updated_at: Timestamp,
}

fn short_ref(what: &str, v: Option<&String>) -> Result<(), RepoError> {
    if let Some(s) = v
        && (s.trim() != s || s.is_empty() || s.chars().count() > MEASUREMENT_REF_MAX)
    {
        return Err(invalid(format!(
            "{what} must be 1..={MEASUREMENT_REF_MAX} characters without surrounding whitespace"
        )));
    }
    Ok(())
}

impl MeasurementProvenance {
    /// Checks the stamp's own limits.
    pub fn validate(&self) -> Result<(), RepoError> {
        short_ref("provenance device_id", self.device_id.as_ref())?;
        short_ref("provenance actor", self.actor.as_ref())?;
        if !(self.center_hz.is_finite() && self.center_hz >= 0.0) {
            return Err(invalid(
                "provenance center_hz must be a finite, non-negative Hz",
            ));
        }
        if !(self.span_hz.is_finite() && self.span_hz > 0.0) {
            return Err(invalid("provenance span_hz must be a finite, positive Hz"));
        }
        if self
            .sample_rate_hz
            .is_some_and(|r| !(r.is_finite() && r > 0.0))
        {
            return Err(invalid(
                "provenance sample_rate_hz must be finite and positive",
            ));
        }
        if self.t_capture[1] < self.t_capture[0] {
            return Err(invalid(
                "provenance t_capture must be [start, end] with start <= end",
            ));
        }
        if !self.authored {
            return Err(invalid("a measurement's provenance has authored: true"));
        }
        Ok(())
    }
}

impl Measurement {
    /// Builds a measurement from its inputs, computing value, unit and place.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: MeasurementId,
        kind: MeasurementKind,
        cursors: Vec<MeasurementCursor>,
        n: Option<u32>,
        collection_id: Option<String>,
        note: Option<String>,
        provenance: MeasurementProvenance,
        now: Timestamp,
    ) -> Result<Self, RepoError> {
        let c = compute_measurement(kind, &cursors, n)?;
        let m = Self {
            id,
            collection_id,
            kind,
            value: c.value,
            unit: c.unit.to_owned(),
            basis: c.basis,
            f_lo_hz: c.f_lo_hz,
            f_hi_hz: c.f_hi_hz,
            t0: c.t0,
            t1: c.t1,
            cursors,
            n,
            note,
            provenance,
            created_at: now,
            updated_at: now,
        };
        m.validate()?;
        Ok(m)
    }

    /// Re-derives value, unit and place from the current `kind`/`cursors`/`n` (after an edit).
    pub fn recompute(&mut self) -> Result<(), RepoError> {
        let c = compute_measurement(self.kind, &self.cursors, self.n)?;
        self.value = c.value;
        self.unit = c.unit.to_owned();
        self.basis = c.basis;
        self.f_lo_hz = c.f_lo_hz;
        self.f_hi_hz = c.f_hi_hz;
        self.t0 = c.t0;
        self.t1 = c.t1;
        Ok(())
    }

    /// Checks every limit, and that the stored value and place are exactly
    /// [`compute_measurement`]'s — so no writer can file a value it computed itself.
    pub fn validate(&self) -> Result<(), RepoError> {
        let c = compute_measurement(self.kind, &self.cursors, self.n)?;
        if (c.value, c.unit, c.basis, c.f_lo_hz, c.f_hi_hz, c.t0, c.t1)
            != (
                self.value,
                self.unit.as_str(),
                self.basis,
                self.f_lo_hz,
                self.f_hi_hz,
                self.t0,
                self.t1,
            )
        {
            return Err(invalid(
                "a measurement's value, unit and place are computed from its cursors, never \
                 supplied",
            ));
        }
        if self
            .note
            .as_ref()
            .is_some_and(|b| b.chars().count() > MEASUREMENT_NOTE_MAX)
        {
            return Err(invalid(format!(
                "measurement note must be at most {MEASUREMENT_NOTE_MAX} characters"
            )));
        }
        if let Some(c) = &self.collection_id
            && c.parse::<uuid::Uuid>().is_err()
        {
            return Err(invalid("measurement collection_id must be a UUID"));
        }
        self.provenance.validate()?;
        if self.updated_at < self.created_at {
            return Err(invalid("measurement updated_at is before created_at"));
        }
        Ok(())
    }
}

/// The optional filters of [`Repository::measurements_in`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MeasurementFilter {
    /// Only this collection's measurements.
    pub collection_id: Option<String>,
    /// Only those whose place intersects `[f_lo, f_hi] × [t0, t1]` (closed on both axes).
    pub window: Option<(f64, f64, Timestamp, Timestamp)>,
}

/// A page of [`Repository::measurements_in`].
#[derive(Clone, Debug, PartialEq)]
pub struct MeasurementPage {
    /// The rows, newest capture time first.
    pub rows: Vec<Measurement>,
    /// How many rows the whole filter matches.
    pub matched: u64,
}

impl Repository {
    fn ensure_measurement_table(&self) -> Result<(), RepoError> {
        self.conn.execute_batch(ENSURE_TABLE)?;
        Ok(())
    }

    /// Stores a new measurement (validated). Its id must be new ([`RepoError::Engine`] otherwise;
    /// check with [`Repository::measurement`] first to tell a duplicate apart).
    pub fn insert_measurement(&mut self, m: &Measurement) -> Result<(), RepoError> {
        m.validate()?;
        self.ensure_measurement_table()?;
        let tx = self.write_tx()?;
        tx.execute(
            "INSERT INTO saved_measurement (measurement_id, collection_id, f_lo, f_hi, t0, t1, \
             created_at, updated_at, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                blob(m.id),
                m.collection_id,
                m.f_lo_hz,
                m.f_hi_hz,
                m.t0.as_unix_nanos(),
                m.t1.as_unix_nanos(),
                m.created_at.as_unix_nanos(),
                m.updated_at.as_unix_nanos(),
                serde_json::to_string(m)?
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Replaces a stored measurement (validated, so its value must be the recomputed one).
    /// `created_at` is kept from the stored row; `updated_at` never goes below it.
    pub fn update_measurement(&mut self, m: &Measurement) -> Result<Measurement, RepoError> {
        let stored = self.measurement(m.id)?;
        let mut next = m.clone();
        next.created_at = stored.created_at;
        if next.updated_at < next.created_at {
            next.updated_at = next.created_at;
        }
        next.validate()?;
        let tx = self.write_tx()?;
        tx.execute(
            "UPDATE saved_measurement SET collection_id = ?2, f_lo = ?3, f_hi = ?4, t0 = ?5, \
             t1 = ?6, updated_at = ?7, body = ?8 WHERE measurement_id = ?1",
            params![
                blob(next.id),
                next.collection_id,
                next.f_lo_hz,
                next.f_hi_hz,
                next.t0.as_unix_nanos(),
                next.t1.as_unix_nanos(),
                next.updated_at.as_unix_nanos(),
                serde_json::to_string(&next)?
            ],
        )?;
        tx.commit()?;
        Ok(next)
    }

    /// Deletes a measurement; returns what was deleted.
    pub fn delete_measurement(&mut self, id: MeasurementId) -> Result<Measurement, RepoError> {
        let stored = self.measurement(id)?;
        let tx = self.write_tx()?;
        tx.execute(
            "DELETE FROM saved_measurement WHERE measurement_id = ?1",
            [blob(id)],
        )?;
        tx.commit()?;
        Ok(stored)
    }

    /// One measurement.
    pub fn measurement(&self, id: MeasurementId) -> Result<Measurement, RepoError> {
        self.ensure_measurement_table()?;
        let body: Option<String> = self
            .conn
            .prepare_cached("SELECT body FROM saved_measurement WHERE measurement_id = ?1")?
            .query_row([blob(id)], |r| r.get(0))
            .optional()?;
        match body {
            Some(b) => Ok(serde_json::from_str(&b)?),
            None => Err(RepoError::NotFound {
                kind: "measurement",
                id: id.to_string(),
            }),
        }
    }

    /// The measurements matching `filter`, newest capture time first (`t1`, then `t0`, then id —
    /// a total order, so paging is stable), skipping `offset` and returning at most `limit`
    /// (capped at [`MEASUREMENT_PAGE_MAX`]), plus how many the whole filter matches.
    pub fn measurements_in(
        &self,
        filter: &MeasurementFilter,
        offset: usize,
        limit: usize,
    ) -> Result<MeasurementPage, RepoError> {
        self.ensure_measurement_table()?;
        // Each filter is a no-op when its parameter is NULL, so one statement serves all four
        // combinations and stays cacheable.
        const WHERE: &str = "(?1 IS NULL OR collection_id = ?1) AND \
             (?2 IS NULL OR (f_lo <= ?3 AND f_hi >= ?2 AND t0 <= ?5 AND t1 >= ?4))";
        let (lo, hi, a, b) = match filter.window {
            Some((lo, hi, a, b)) => (
                Some(lo),
                Some(hi),
                Some(a.as_unix_nanos()),
                Some(b.as_unix_nanos()),
            ),
            None => (None, None, None, None),
        };
        let coll = filter.collection_id.as_deref();
        let matched: i64 = self
            .conn
            .prepare_cached(&format!(
                "SELECT COUNT(*) FROM saved_measurement WHERE {WHERE}"
            ))?
            .query_row(params![coll, lo, hi, a, b], |r| r.get(0))?;
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT body FROM saved_measurement WHERE {WHERE} \
             ORDER BY t1 DESC, t0 DESC, measurement_id LIMIT ?6 OFFSET ?7"
        ))?;
        let texts = stmt
            .query_map(
                params![
                    coll,
                    lo,
                    hi,
                    a,
                    b,
                    limit.min(MEASUREMENT_PAGE_MAX) as i64,
                    offset as i64
                ],
                |r| r.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let rows = texts
            .iter()
            .map(|t| serde_json::from_str(t).map_err(RepoError::from))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(MeasurementPage {
            rows,
            matched: matched.max(0) as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: f64) -> Timestamp {
        Timestamp::from_unix_nanos((s * 1e9).round() as i64)
    }

    fn cur(f_hz: f64, t: f64) -> MeasurementCursor {
        MeasurementCursor { f_hz, t: ts(t) }
    }

    fn prov() -> MeasurementProvenance {
        MeasurementProvenance {
            device_id: Some("hackrf:0001".into()),
            center_hz: 433.92e6,
            span_hz: 2e6,
            sample_rate_hz: Some(2e6),
            t_capture: [ts(990.0), ts(1010.0)],
            tier: MeasurementTier::LiveIq,
            authored_at: Timestamp::now(),
            actor: Some("tok-abc".into()),
            authored: true,
        }
    }

    fn take(kind: MeasurementKind, c: [MeasurementCursor; 2], n: Option<u32>) -> Measurement {
        Measurement::new(
            MeasurementId::new(),
            kind,
            c.to_vec(),
            n,
            None,
            None,
            prov(),
            Timestamp::now(),
        )
        .unwrap()
    }

    #[test]
    fn every_kind_computes_its_value_unit_and_place() {
        // Cursors given out of order: the place is still lo..hi, first..last.
        let c = [cur(433.95e6, 1002.0), cur(433.90e6, 1000.0)];
        let df = take(MeasurementKind::DeltaF, c, None);
        assert!((df.value - 50e3).abs() < 1e-3, "{}", df.value);
        assert_eq!(df.unit, "Hz");
        assert_eq!((df.f_lo_hz, df.f_hi_hz), (433.90e6, 433.95e6));
        assert_eq!((df.t0, df.t1), (ts(1000.0), ts(1002.0)));
        assert_eq!(df.basis, MeasurementBasis::Cursors);
        let dt = take(MeasurementKind::DeltaT, c, None);
        assert_eq!((dt.value, dt.unit.as_str()), (2.0, "s"));
        assert_eq!(take(MeasurementKind::Bandwidth, c, None).unit, "Hz");
        assert_eq!(take(MeasurementKind::Duration, c, None).value, 2.0);
        // 10 symbols over 2 ms: 5 kBd, period 200 µs — the reciprocal pair.
        let s = [cur(433.92e6, 1000.0), cur(433.92e6, 1000.002)];
        let rate = take(MeasurementKind::SymbolRate, s, Some(10));
        assert_eq!(rate.unit, "Bd");
        assert!((rate.value - 5000.0).abs() < 1e-6, "{}", rate.value);
        let period = take(MeasurementKind::Period, s, Some(10));
        assert_eq!(period.unit, "s");
        assert!((period.value - 200e-6).abs() < 1e-12, "{}", period.value);
    }

    #[test]
    fn bad_inputs_and_supplied_values_are_refused() {
        let same_f = [cur(1e6, 0.0), cur(1e6, 1.0)];
        let same_t = [cur(1e6, 0.0), cur(2e6, 0.0)];
        let refuse = |k, c: [MeasurementCursor; 2], n| compute_measurement(k, &c, n).is_err();
        assert!(refuse(MeasurementKind::Bandwidth, same_f, None));
        assert!(refuse(MeasurementKind::Duration, same_t, None));
        assert!(refuse(MeasurementKind::SymbolRate, same_t, Some(4)));
        assert!(refuse(MeasurementKind::SymbolRate, same_f, None));
        assert!(refuse(MeasurementKind::Period, same_f, Some(0)));
        assert!(refuse(MeasurementKind::DeltaF, same_t, Some(3)));
        assert!(refuse(
            MeasurementKind::DeltaF,
            [cur(-1.0, 0.0), cur(1.0, 0.0)],
            None
        ));
        assert!(compute_measurement(MeasurementKind::DeltaF, &[cur(1.0, 0.0)], None).is_err());
        // Zero-width Δf/Δt are legitimate readings.
        assert!(!refuse(MeasurementKind::DeltaF, same_f, None));

        let mut m = take(MeasurementKind::DeltaF, same_t, None);
        m.value = 12_345.0;
        assert!(m.validate().is_err(), "a value it did not compute");
        let mut m = take(MeasurementKind::DeltaF, same_t, None);
        m.unit = "kHz".into();
        assert!(m.validate().is_err());
        let mut m = take(MeasurementKind::DeltaF, same_t, None);
        m.collection_id = Some("not-a-uuid".into());
        assert!(m.validate().is_err());
        let mut m = take(MeasurementKind::DeltaF, same_t, None);
        m.provenance.authored = false;
        assert!(m.validate().is_err());
    }

    #[test]
    fn round_trip_update_delete_and_filtered_paging() {
        let mut repo = Repository::open_in_memory().unwrap();
        let coll = uuid::Uuid::now_v7().to_string();
        let a = take(
            MeasurementKind::DeltaF,
            [cur(100e6, 1000.0), cur(100.2e6, 1001.0)],
            None,
        );
        let mut b = take(
            MeasurementKind::Duration,
            [cur(100.1e6, 1005.0), cur(100.1e6, 1006.0)],
            None,
        );
        b.collection_id = Some(coll.clone());
        let far = take(
            MeasurementKind::DeltaT,
            [cur(433.9e6, 1005.0), cur(433.9e6, 1007.0)],
            None,
        );
        for m in [&a, &b, &far] {
            repo.insert_measurement(m).unwrap();
        }
        assert_eq!(repo.measurement(a.id).unwrap(), a);

        let all = repo
            .measurements_in(&MeasurementFilter::default(), 0, 10)
            .unwrap();
        assert_eq!(all.matched, 3);
        assert_eq!(
            all.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![far.id, b.id, a.id],
            "newest capture time first"
        );
        let windowed = MeasurementFilter {
            collection_id: None,
            window: Some((99e6, 101e6, ts(900.0), ts(2000.0))),
        };
        let page = repo.measurements_in(&windowed, 0, 1).unwrap();
        assert_eq!((page.matched, page.rows[0].id), (2, b.id));
        let page2 = repo.measurements_in(&windowed, 1, 1).unwrap();
        assert_eq!(page2.rows[0].id, a.id);
        let in_coll = MeasurementFilter {
            collection_id: Some(coll),
            window: None,
        };
        let c = repo.measurements_in(&in_coll, 0, 10).unwrap();
        assert_eq!((c.matched, c.rows[0].id), (1, b.id));

        // Moving a cursor recomputes; the stored value follows.
        let mut moved = a.clone();
        moved.cursors[1].f_hz = 100.5e6;
        moved.recompute().unwrap();
        moved.updated_at = Timestamp::now();
        let saved = repo.update_measurement(&moved).unwrap();
        assert!((saved.value - 500e3).abs() < 1e-3);
        assert_eq!(saved.created_at, a.created_at);
        assert_eq!(repo.measurement(a.id).unwrap().f_hi_hz, 100.5e6);

        assert_eq!(repo.delete_measurement(a.id).unwrap().id, a.id);
        assert!(matches!(
            repo.measurement(a.id),
            Err(RepoError::NotFound { .. })
        ));
    }
}
