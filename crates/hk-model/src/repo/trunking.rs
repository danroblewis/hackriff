//! C23 trunking storage (T-266): systems, calls and the grant stream, over migration 0013.
//! Types: [`crate::trunking`].
//!
//! Three rules this module keeps, on top of the migration's CHECKs and triggers:
//!
//! - **Encryption is never defaulted, on the way in or on the way out.** The write path projects
//!   [`Encryption`] into `(state, evidence, algid, key_id)` — a state that makes a claim always
//!   carries its evidence — and the read path ([`enc_from_columns`]) *fails* on a contradictory or
//!   unrecognised row rather than falling back to a value. There is no `_ => Clear` arm anywhere in
//!   this file, which is the whole point: an unreadable row must be an error, because the one
//!   default that would look harmless is exactly the dangerous one.
//! - **A call never walks back towards clear.** [`Repository::put_call`] refuses to overwrite an
//!   `encrypted` call with `clear` or `unknown`, and the schema's trigger refuses it again.
//! - **Nothing here stores content.** No audio, no voice frames, no message payload — M4 is
//!   metadata only, and `no_audio_column_exists` asserts the schema keeps it that way.

use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use super::{RepoError, Repository, blob, opt_blob};
use crate::ids::{CallRecordId, EmitterId, TrunkSystemId};
use crate::time::Timestamp;
use crate::trunking::{
    CallRecord, ChannelPlanEntry, Encryption, EncryptionEvidence, GrantEvent, GrantKind,
    InvalidTrunking, LabelSource, NeighbourSite, Talkgroup, TrunkProtocol, TrunkSystem,
};

fn invalid(e: InvalidTrunking) -> RepoError {
    RepoError::Invalid(e.0)
}

fn ts(nanos: i64) -> Timestamp {
    Timestamp::from_unix_nanos(nanos)
}

// ---------------------------------------------------------------------------------------------
// The encryption codec: the only place the three-state column is read or written
// ---------------------------------------------------------------------------------------------

/// `(state, evidence, algid, key_id)` — the four columns an [`Encryption`] projects onto.
type EncColumns = (&'static str, Option<&'static str>, Option<i64>, Option<i64>);

/// Projects an [`Encryption`] onto its columns. A state that makes a claim always brings its
/// evidence with it, so the schema's paired CHECK can never fail for a value built in Rust.
fn enc_columns(e: Encryption) -> EncColumns {
    (
        e.state(),
        e.evidence().map(EncryptionEvidence::as_str),
        e.algid().map(i64::from),
        e.key_id().map(i64::from),
    )
}

/// Reads the four columns back. **Every** inconsistency is an error: a claim with no evidence, an
/// `unknown` carrying evidence, an unrecognised state. Nothing here produces a value by default —
/// in particular nothing produces `clear`.
fn enc_from_columns(
    state: &str,
    evidence: Option<String>,
    algid: Option<i64>,
    key_id: Option<i64>,
) -> Result<Encryption, RepoError> {
    let evidence = match evidence {
        Some(text) => Some(EncryptionEvidence::parse(&text).map_err(invalid)?),
        None => None,
    };
    let algid = match algid {
        Some(a) => Some(u8::try_from(a).map_err(|_| RepoError::Invalid(format!("ALGID {a}")))?),
        None => None,
    };
    let key_id = match key_id {
        Some(k) => Some(u16::try_from(k).map_err(|_| RepoError::Invalid(format!("key id {k}")))?),
        None => None,
    };
    let enc = match (state, evidence) {
        ("unknown", None) => Encryption::Unknown,
        ("unknown", Some(_)) => {
            return Err(RepoError::Invalid(
                "an unknown encryption state cannot name evidence".into(),
            ));
        }
        ("clear", Some(evidence)) => Encryption::Clear {
            evidence,
            algid,
            key_id,
        },
        ("encrypted", Some(evidence)) => Encryption::Encrypted {
            evidence,
            algid,
            key_id,
        },
        ("clear" | "encrypted", None) => {
            return Err(RepoError::Invalid(format!(
                "a stored '{state}' encryption state names no evidence; refusing to read it"
            )));
        }
        (other, _) => {
            return Err(RepoError::Invalid(format!(
                "{other} is not an encryption state"
            )));
        }
    };
    enc.validate().map_err(invalid)?;
    Ok(enc)
}

// ---------------------------------------------------------------------------------------------
// Row shapes
// ---------------------------------------------------------------------------------------------

const CALL_COLUMNS: &str = "call_id, trunk_system_id, t_start, t_end, talkgroup, unit_id, \
     channel, slot, f_hz, encryption, encryption_evidence, algid, key_id, late_entry, \
     emitter_id, reasons";

struct RawCall {
    id: [u8; 16],
    system: [u8; 16],
    t_start: i64,
    t_end: Option<i64>,
    talkgroup: Option<String>,
    unit_id: Option<String>,
    channel: Option<String>,
    slot: Option<i64>,
    f_hz: Option<f64>,
    state: String,
    evidence: Option<String>,
    algid: Option<i64>,
    key_id: Option<i64>,
    late_entry: i64,
    emitter_id: Option<[u8; 16]>,
    reasons: String,
}

fn raw_call(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawCall> {
    Ok(RawCall {
        id: r.get(0)?,
        system: r.get(1)?,
        t_start: r.get(2)?,
        t_end: r.get(3)?,
        talkgroup: r.get(4)?,
        unit_id: r.get(5)?,
        channel: r.get(6)?,
        slot: r.get(7)?,
        f_hz: r.get(8)?,
        state: r.get(9)?,
        evidence: r.get(10)?,
        algid: r.get(11)?,
        key_id: r.get(12)?,
        late_entry: r.get(13)?,
        emitter_id: r.get(14)?,
        reasons: r.get(15)?,
    })
}

fn call_from(raw: RawCall) -> Result<CallRecord, RepoError> {
    Ok(CallRecord {
        id: CallRecordId::from_uuid(Uuid::from_bytes(raw.id)),
        system: TrunkSystemId::from_uuid(Uuid::from_bytes(raw.system)),
        t_start: ts(raw.t_start),
        t_end: raw.t_end.map(ts),
        talkgroup: raw.talkgroup,
        unit_id: raw.unit_id,
        channel: raw.channel,
        slot: raw
            .slot
            .map(|s| u8::try_from(s).map_err(|_| RepoError::Invalid(format!("slot {s}"))))
            .transpose()?,
        f_hz: raw.f_hz,
        encryption: enc_from_columns(&raw.state, raw.evidence, raw.algid, raw.key_id)?,
        late_entry: raw.late_entry != 0,
        emitter_id: raw
            .emitter_id
            .map(|b| EmitterId::from_uuid(Uuid::from_bytes(b))),
        reasons: serde_json::from_str(&raw.reasons)?,
    })
}

const GRANT_COLUMNS: &str = "trunk_system_id, call_id, kind, t, talkgroup, unit_id, channel, \
     slot, f_hz, encryption, encryption_evidence, algid, key_id, detail";

struct RawGrant {
    system: [u8; 16],
    call: Option<[u8; 16]>,
    kind: String,
    t: i64,
    talkgroup: Option<String>,
    unit_id: Option<String>,
    channel: Option<String>,
    slot: Option<i64>,
    f_hz: Option<f64>,
    state: String,
    evidence: Option<String>,
    algid: Option<i64>,
    key_id: Option<i64>,
    detail: String,
}

fn raw_grant(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawGrant> {
    Ok(RawGrant {
        system: r.get(0)?,
        call: r.get(1)?,
        kind: r.get(2)?,
        t: r.get(3)?,
        talkgroup: r.get(4)?,
        unit_id: r.get(5)?,
        channel: r.get(6)?,
        slot: r.get(7)?,
        f_hz: r.get(8)?,
        state: r.get(9)?,
        evidence: r.get(10)?,
        algid: r.get(11)?,
        key_id: r.get(12)?,
        detail: r.get(13)?,
    })
}

fn grant_from(raw: RawGrant) -> Result<GrantEvent, RepoError> {
    Ok(GrantEvent {
        system: TrunkSystemId::from_uuid(Uuid::from_bytes(raw.system)),
        call: raw
            .call
            .map(|b| CallRecordId::from_uuid(Uuid::from_bytes(b))),
        kind: GrantKind::parse(&raw.kind).map_err(invalid)?,
        t: ts(raw.t),
        talkgroup: raw.talkgroup,
        unit_id: raw.unit_id,
        channel: raw.channel,
        slot: raw
            .slot
            .map(|s| u8::try_from(s).map_err(|_| RepoError::Invalid(format!("slot {s}"))))
            .transpose()?,
        f_hz: raw.f_hz,
        encryption: enc_from_columns(&raw.state, raw.evidence, raw.algid, raw.key_id)?,
        detail: serde_json::from_str(&raw.detail)?,
    })
}

fn no_such_system(id: TrunkSystemId) -> RepoError {
    RepoError::NotFound {
        kind: "trunk system",
        id: id.to_string(),
    }
}

// ---------------------------------------------------------------------------------------------
// Repository API
// ---------------------------------------------------------------------------------------------

impl Repository {
    /// Inserts or updates a system's identity row. The channel table, neighbours and talkgroups are
    /// written through their own methods (they are append-only history and an aggregate
    /// respectively); a read fills the struct's vectors back in.
    pub fn put_trunk_system(&mut self, s: &TrunkSystem) -> Result<(), RepoError> {
        s.validate().map_err(invalid)?;
        self.conn.execute(
            "INSERT INTO trunk_system (trunk_system_id, protocol, system_id, site_id, cc_freq_hz, \
             first_seen, last_seen, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT (trunk_system_id) DO UPDATE SET \
             protocol = excluded.protocol, system_id = excluded.system_id, \
             site_id = excluded.site_id, cc_freq_hz = excluded.cc_freq_hz, \
             first_seen = min(trunk_system.first_seen, excluded.first_seen), \
             last_seen = max(trunk_system.last_seen, excluded.last_seen), \
             updated_at = excluded.updated_at",
            params![
                blob(s.id),
                s.protocol.as_str(),
                s.system_id,
                s.site_id,
                s.cc_freq_hz,
                s.first_seen.as_unix_nanos(),
                s.last_seen.as_unix_nanos(),
                s.created_at.as_unix_nanos(),
                s.updated_at.as_unix_nanos(),
            ],
        )?;
        Ok(())
    }

    /// One system with its current channel table, neighbours and talkgroups.
    pub fn trunk_system(&self, id: TrunkSystemId) -> Result<TrunkSystem, RepoError> {
        self.trunk_system_opt(id)?.ok_or_else(|| no_such_system(id))
    }

    /// One system, or `None`.
    pub fn trunk_system_opt(&self, id: TrunkSystemId) -> Result<Option<TrunkSystem>, RepoError> {
        struct Row {
            protocol: String,
            system_id: Option<String>,
            site_id: Option<String>,
            cc_freq_hz: Option<f64>,
            first_seen: i64,
            last_seen: i64,
            created_at: i64,
            updated_at: i64,
        }
        let row: Option<Row> = self
            .conn
            .prepare_cached(
                "SELECT protocol, system_id, site_id, cc_freq_hz, first_seen, last_seen, \
                     created_at, updated_at FROM trunk_system WHERE trunk_system_id = ?1",
            )?
            .query_row(params![blob(id)], |r| {
                Ok(Row {
                    protocol: r.get(0)?,
                    system_id: r.get(1)?,
                    site_id: r.get(2)?,
                    cc_freq_hz: r.get(3)?,
                    first_seen: r.get(4)?,
                    last_seen: r.get(5)?,
                    created_at: r.get(6)?,
                    updated_at: r.get(7)?,
                })
            })
            .optional()?;
        let Some(row) = row else {
            return Ok(None);
        };
        let (first, last, created, updated) = (
            row.first_seen,
            row.last_seen,
            row.created_at,
            row.updated_at,
        );
        Ok(Some(TrunkSystem {
            id,
            protocol: TrunkProtocol::parse(&row.protocol).map_err(invalid)?,
            system_id: row.system_id,
            site_id: row.site_id,
            cc_freq_hz: row.cc_freq_hz,
            channel_plan: self.channel_plan(id)?,
            neighbours: self.trunk_neighbours(id)?,
            talkgroups: self.talkgroups(id)?,
            first_seen: ts(first),
            last_seen: ts(last),
            created_at: ts(created),
            updated_at: ts(updated),
        }))
    }

    /// Every system, oldest id first.
    pub fn trunk_systems(&self) -> Result<Vec<TrunkSystem>, RepoError> {
        let ids: Vec<[u8; 16]> = self
            .conn
            .prepare_cached("SELECT trunk_system_id FROM trunk_system ORDER BY trunk_system_id")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|b| self.trunk_system(TrunkSystemId::from_uuid(Uuid::from_bytes(b))))
            .collect()
    }

    /// Looks a system up by its natural key. Undecoded identifiers (`None`) match only other
    /// undecoded ones — nothing is merged on the strength of not knowing.
    pub fn find_trunk_system(
        &self,
        protocol: TrunkProtocol,
        system_id: Option<&str>,
        site_id: Option<&str>,
    ) -> Result<Option<TrunkSystemId>, RepoError> {
        let found: Option<[u8; 16]> = self
            .conn
            .prepare_cached(
                "SELECT trunk_system_id FROM trunk_system \
                 WHERE protocol = ?1 AND system_id IS ?2 AND site_id IS ?3",
            )?
            .query_row(params![protocol.as_str(), system_id, site_id], |r| r.get(0))
            .optional()?;
        Ok(found.map(|b| TrunkSystemId::from_uuid(Uuid::from_bytes(b))))
    }

    /// Appends a channel-table entry as decoded. Never an update: the history is what makes a stale
    /// table detectable instead of silently mapping a grant to the wrong frequency.
    pub fn append_channel_plan(
        &mut self,
        system: TrunkSystemId,
        entry: &ChannelPlanEntry,
    ) -> Result<(), RepoError> {
        entry.validate().map_err(invalid)?;
        self.conn.execute(
            "INSERT INTO trunk_channel_plan (trunk_system_id, iden, base_hz, spacing_hz, \
             tx_offset_hz, bandwidth_hz, slots, t) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                blob(system),
                i64::from(entry.iden),
                entry.base_hz,
                entry.spacing_hz,
                entry.tx_offset_hz,
                entry.bandwidth_hz,
                i64::from(entry.slots),
                entry.t.as_unix_nanos(),
            ],
        )?;
        Ok(())
    }

    /// The current channel table: the newest entry per `iden`, by `iden`.
    pub fn channel_plan(&self, system: TrunkSystemId) -> Result<Vec<ChannelPlanEntry>, RepoError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT iden, base_hz, spacing_hz, tx_offset_hz, bandwidth_hz, slots, t \
             FROM trunk_channel_plan c WHERE c.trunk_system_id = ?1 AND NOT EXISTS ( \
               SELECT 1 FROM trunk_channel_plan c2 WHERE c2.trunk_system_id = c.trunk_system_id \
               AND c2.iden = c.iden AND (c2.t > c.t OR (c2.t = c.t AND c2.entry_id > c.entry_id))) \
             ORDER BY c.iden",
        )?;
        let rows = stmt
            .query_map(params![blob(system)], plan_entry)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every entry ever decoded for one `iden`, newest first — how a reader tells a fresh table
    /// from a stale one.
    pub fn channel_plan_history(
        &self,
        system: TrunkSystemId,
        iden: u8,
        limit: u32,
    ) -> Result<Vec<ChannelPlanEntry>, RepoError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT iden, base_hz, spacing_hz, tx_offset_hz, bandwidth_hz, slots, t \
             FROM trunk_channel_plan WHERE trunk_system_id = ?1 AND iden = ?2 \
             ORDER BY t DESC, entry_id DESC LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(
                params![blob(system), i64::from(iden), i64::from(limit)],
                plan_entry,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Appends a neighbour-site announcement.
    pub fn append_trunk_neighbour(
        &mut self,
        system: TrunkSystemId,
        n: &NeighbourSite,
    ) -> Result<(), RepoError> {
        n.validate().map_err(invalid)?;
        self.conn.execute(
            "INSERT INTO trunk_neighbour (trunk_system_id, site_id, cc_freq_hz, t) \
             VALUES (?1, ?2, ?3, ?4)",
            params![blob(system), n.site_id, n.cc_freq_hz, n.t.as_unix_nanos()],
        )?;
        Ok(())
    }

    /// Every neighbour announcement for a system, oldest first.
    pub fn trunk_neighbours(&self, system: TrunkSystemId) -> Result<Vec<NeighbourSite>, RepoError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT site_id, cc_freq_hz, t FROM trunk_neighbour WHERE trunk_system_id = ?1 \
             ORDER BY neighbour_id",
        )?;
        let rows = stmt
            .query_map(params![blob(system)], |r| {
                Ok(NeighbourSite {
                    site_id: r.get(0)?,
                    cc_freq_hz: r.get(1)?,
                    t: ts(r.get(2)?),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Inserts or updates a talkgroup. The seen window widens and `calls` only grows, so replaying
    /// the same traffic cannot shrink either.
    pub fn upsert_talkgroup(
        &mut self,
        system: TrunkSystemId,
        tg: &Talkgroup,
    ) -> Result<(), RepoError> {
        tg.validate().map_err(invalid)?;
        self.conn.execute(
            "INSERT INTO trunk_talkgroup (trunk_system_id, talkgroup, label, label_source, \
             first_seen, last_seen, calls) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT (trunk_system_id, talkgroup) DO UPDATE SET \
             label = excluded.label, label_source = excluded.label_source, \
             first_seen = min(trunk_talkgroup.first_seen, excluded.first_seen), \
             last_seen = max(trunk_talkgroup.last_seen, excluded.last_seen), \
             calls = max(trunk_talkgroup.calls, excluded.calls)",
            params![
                blob(system),
                tg.id,
                tg.label,
                tg.label_source.map(LabelSource::as_str),
                tg.first_seen.as_unix_nanos(),
                tg.last_seen.as_unix_nanos(),
                i64::try_from(tg.calls)
                    .map_err(|_| RepoError::Invalid("talkgroup call count exceeds i64".into()))?,
            ],
        )?;
        Ok(())
    }

    /// A system's talkgroups, by id.
    pub fn talkgroups(&self, system: TrunkSystemId) -> Result<Vec<Talkgroup>, RepoError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT talkgroup, label, label_source, first_seen, last_seen, calls \
             FROM trunk_talkgroup WHERE trunk_system_id = ?1 ORDER BY talkgroup",
        )?;
        let rows = stmt
            .query_map(params![blob(system)], |r| {
                let source: Option<String> = r.get(2)?;
                let calls: i64 = r.get(5)?;
                Ok((
                    Talkgroup {
                        id: r.get(0)?,
                        label: r.get(1)?,
                        label_source: None,
                        first_seen: ts(r.get(3)?),
                        last_seen: ts(r.get(4)?),
                        calls: calls.max(0).unsigned_abs(),
                    },
                    source,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(mut tg, source)| {
                tg.label_source = match source {
                    Some(s) => Some(LabelSource::parse(&s).map_err(invalid)?),
                    None => None,
                };
                Ok(tg)
            })
            .collect()
    }

    /// Inserts or updates a call. **Refuses to walk an `encrypted` call back to `clear` or
    /// `unknown`**: a mid-call key change must never read as "listenable after all". The schema's
    /// trigger refuses it again.
    pub fn put_call(&mut self, c: &CallRecord) -> Result<(), RepoError> {
        c.validate().map_err(invalid)?;
        if let Some(existing) = self.call_encryption_state(c.id)?
            && existing == "encrypted"
            && !c.encryption.is_encrypted()
        {
            return Err(RepoError::Invalid(format!(
                "call {} was seen encrypted; it cannot be rewritten as {}",
                c.id,
                c.encryption.state()
            )));
        }
        let (state, evidence, algid, key_id) = enc_columns(c.encryption);
        self.conn.execute(
            "INSERT INTO call_record (call_id, trunk_system_id, t_start, t_end, talkgroup, \
             unit_id, channel, slot, f_hz, encryption, encryption_evidence, algid, key_id, \
             late_entry, emitter_id, reasons) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16) \
             ON CONFLICT (call_id) DO UPDATE SET \
             t_end = excluded.t_end, talkgroup = excluded.talkgroup, unit_id = excluded.unit_id, \
             channel = excluded.channel, slot = excluded.slot, f_hz = excluded.f_hz, \
             encryption = excluded.encryption, \
             encryption_evidence = excluded.encryption_evidence, algid = excluded.algid, \
             key_id = excluded.key_id, late_entry = excluded.late_entry, \
             emitter_id = excluded.emitter_id, reasons = excluded.reasons",
            params![
                blob(c.id),
                blob(c.system),
                c.t_start.as_unix_nanos(),
                c.t_end.map(|t| t.as_unix_nanos()),
                c.talkgroup,
                c.unit_id,
                c.channel,
                c.slot.map(i64::from),
                c.f_hz,
                state,
                evidence,
                algid,
                key_id,
                i64::from(c.late_entry),
                opt_blob(c.emitter_id),
                serde_json::to_string(&c.reasons)?,
            ],
        )?;
        Ok(())
    }

    fn call_encryption_state(&self, id: CallRecordId) -> Result<Option<String>, RepoError> {
        Ok(self
            .conn
            .prepare_cached("SELECT encryption FROM call_record WHERE call_id = ?1")?
            .query_row(params![blob(id)], |r| r.get(0))
            .optional()?)
    }

    /// One call by id.
    pub fn call(&self, id: CallRecordId) -> Result<CallRecord, RepoError> {
        self.call_opt(id)?.ok_or(RepoError::NotFound {
            kind: "call record",
            id: id.to_string(),
        })
    }

    /// One call by id, or `None`.
    pub fn call_opt(&self, id: CallRecordId) -> Result<Option<CallRecord>, RepoError> {
        let raw = self
            .conn
            .prepare_cached(&format!(
                "SELECT {CALL_COLUMNS} FROM call_record WHERE call_id = ?1"
            ))?
            .query_row(params![blob(id)], raw_call)
            .optional()?;
        raw.map(call_from).transpose()
    }

    /// A system's calls, newest first.
    pub fn calls_for_system(
        &self,
        system: TrunkSystemId,
        limit: u32,
    ) -> Result<Vec<CallRecord>, RepoError> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {CALL_COLUMNS} FROM call_record WHERE trunk_system_id = ?1 \
             ORDER BY t_start DESC, call_id DESC LIMIT ?2"
        ))?;
        let raws = stmt
            .query_map(params![blob(system), i64::from(limit)], raw_call)?
            .collect::<Result<Vec<_>, _>>()?;
        raws.into_iter().map(call_from).collect()
    }

    /// Calls with no observed end, oldest first.
    pub fn open_calls(&self, system: TrunkSystemId) -> Result<Vec<CallRecord>, RepoError> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {CALL_COLUMNS} FROM call_record WHERE trunk_system_id = ?1 AND t_end IS NULL \
             ORDER BY t_start"
        ))?;
        let raws = stmt
            .query_map(params![blob(system)], raw_call)?
            .collect::<Result<Vec<_>, _>>()?;
        raws.into_iter().map(call_from).collect()
    }

    /// Records a call's end (a silence timeout or an explicit release).
    pub fn close_call(&mut self, id: CallRecordId, t_end: Timestamp) -> Result<(), RepoError> {
        let changed = self.conn.execute(
            "UPDATE call_record SET t_end = ?2 WHERE call_id = ?1 AND t_end IS NULL",
            params![blob(id), t_end.as_unix_nanos()],
        )?;
        if changed == 0 {
            return Err(RepoError::NotFound {
                kind: "open call record",
                id: id.to_string(),
            });
        }
        Ok(())
    }

    /// Appends one control-channel event, returning its id. Append-only: what the control channel
    /// said is a measurement.
    pub fn append_grant(&mut self, g: &GrantEvent) -> Result<i64, RepoError> {
        g.validate().map_err(invalid)?;
        let (state, evidence, algid, key_id) = enc_columns(g.encryption);
        self.conn.execute(
            "INSERT INTO grant_event (trunk_system_id, call_id, kind, t, talkgroup, unit_id, \
             channel, slot, f_hz, encryption, encryption_evidence, algid, key_id, detail) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                blob(g.system),
                opt_blob(g.call),
                g.kind.as_str(),
                g.t.as_unix_nanos(),
                g.talkgroup,
                g.unit_id,
                g.channel,
                g.slot.map(i64::from),
                g.f_hz,
                state,
                evidence,
                algid,
                key_id,
                serde_json::to_string(&g.detail)?,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// A system's grant events from `since` onwards, oldest first — the load index's input
    /// (AWARE-067).
    pub fn grants_for_system(
        &self,
        system: TrunkSystemId,
        since: Timestamp,
        limit: u32,
    ) -> Result<Vec<GrantEvent>, RepoError> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {GRANT_COLUMNS} FROM grant_event WHERE trunk_system_id = ?1 AND t >= ?2 \
             ORDER BY t, event_id LIMIT ?3"
        ))?;
        let raws = stmt
            .query_map(
                params![blob(system), since.as_unix_nanos(), i64::from(limit)],
                raw_grant,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        raws.into_iter().map(grant_from).collect()
    }

    /// The events of one call, oldest first.
    pub fn grants_for_call(
        &self,
        call: CallRecordId,
        limit: u32,
    ) -> Result<Vec<GrantEvent>, RepoError> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {GRANT_COLUMNS} FROM grant_event WHERE call_id = ?1 ORDER BY event_id LIMIT ?2"
        ))?;
        let raws = stmt
            .query_map(params![blob(call), i64::from(limit)], raw_grant)?
            .collect::<Result<Vec<_>, _>>()?;
        raws.into_iter().map(grant_from).collect()
    }
}

fn plan_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<ChannelPlanEntry> {
    let iden: i64 = r.get(0)?;
    Ok(ChannelPlanEntry {
        iden: u8::try_from(iden).unwrap_or(u8::MAX),
        base_hz: r.get(1)?,
        spacing_hz: r.get(2)?,
        tx_offset_hz: r.get(3)?,
        bandwidth_hz: r.get(4)?,
        slots: {
            let slots: i64 = r.get(5)?;
            u8::try_from(slots).unwrap_or(1)
        },
        t: ts(r.get(6)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{Fingerprint, Sighting};
    use crate::ids::TrackId;
    use crate::trunking::P25_ALGID_CLEAR;
    use crate::{LinkTarget, TimeRange};

    fn t(sec: i64) -> Timestamp {
        Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
    }

    fn a_system(r: &mut Repository) -> TrunkSystem {
        let mut s = TrunkSystem::new(TrunkProtocol::P25Phase1, Some(851.0125e6), t(0));
        s.system_id = Some("BEE00:2AB".into());
        s.site_id = Some("1:3".into());
        r.put_trunk_system(&s).unwrap();
        s
    }

    fn an_emitter(r: &mut Repository, f: f64) -> EmitterId {
        r.record_sighting(
            &Sighting {
                source: LinkTarget::Track(TrackId::new()),
                seen: TimeRange::new(t(0), t(1)),
                count: 3,
                f_center_hz: f,
                bandwidth_hz: 12.5e3,
                fingerprint: Some(Fingerprint::new(f, 12.5e3)),
                identity: None,
                context: None,
                classification: None,
                tags: Vec::new(),
            },
            None,
        )
        .unwrap()
        .emitter_id
    }

    /// **The T-266 acceptance test.** A call built from a late-entry grant — one joined without
    /// ever seeing the call's header — reads back `unknown`, not `clear`.
    #[test]
    fn a_call_from_a_late_entry_grant_reads_back_unknown() {
        let mut r = Repository::open_in_memory().unwrap();
        let s = a_system(&mut r);

        // A grant update mid-call: it carries the talkgroup and the channel, and says nothing at
        // all about encryption.
        let mut grant = GrantEvent::new(s.id, GrantKind::GrantUpdate, t(10));
        grant.talkgroup = Some("4242".into());
        grant.channel = Some("1-0123".into());
        grant.f_hz = Some(851.2125e6);
        assert_eq!(grant.encryption, Encryption::Unknown);
        r.append_grant(&grant).unwrap();

        let call = CallRecord::from_grant(&grant, true);
        r.put_call(&call).unwrap();

        let back = r.call(call.id).unwrap();
        assert_eq!(back, call, "the record round-trips unchanged");
        assert_eq!(back.encryption, Encryption::Unknown);
        assert!(back.late_entry);
        assert!(!back.encryption.is_clear(), "unknown is never clear");
        assert!(!back.encryption.is_known());
        assert_eq!(back.encryption.evidence(), None);

        // ...and that is what is actually on disk, in the three-state column.
        let (state, evidence): (String, Option<String>) = r
            .conn
            .query_row(
                "SELECT encryption, encryption_evidence FROM call_record WHERE call_id = ?1",
                params![blob(call.id)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "unknown");
        assert_eq!(evidence, None);
    }

    /// The column has no DEFAULT, so a writer that does not decide gets an error — there is no
    /// value it falls into.
    #[test]
    fn encryption_cannot_be_omitted_and_clear_cannot_be_asserted_without_evidence() {
        let mut r = Repository::open_in_memory().unwrap();
        let s = a_system(&mut r);
        let sys = blob(s.id);
        let id = blob(CallRecordId::new());

        // Omitting the column entirely: NOT NULL with no DEFAULT.
        assert!(
            r.conn
                .execute(
                    "INSERT INTO call_record (call_id, trunk_system_id, t_start, late_entry, \
                     reasons) VALUES (?1, ?2, 1, 1, '[]')",
                    params![id, sys],
                )
                .is_err(),
            "a call with no encryption decision must not be storable"
        );

        // Claiming clear with nothing to back it up.
        assert!(
            r.conn
                .execute(
                    "INSERT INTO call_record (call_id, trunk_system_id, t_start, encryption, \
                     late_entry, reasons) VALUES (?1, ?2, 1, 'clear', 1, '[]')",
                    params![id, sys],
                )
                .is_err(),
            "'clear' must name what said so"
        );

        // ...and dressing an unknown up with evidence.
        assert!(
            r.conn
                .execute(
                    "INSERT INTO call_record (call_id, trunk_system_id, t_start, encryption, \
                     encryption_evidence, late_entry, reasons) \
                     VALUES (?1, ?2, 1, 'unknown', 'algid', 1, '[]')",
                    params![id, sys],
                )
                .is_err(),
            "'unknown' cannot carry evidence"
        );

        // A clear row cannot carry an encrypting ALGID.
        assert!(
            r.conn
                .execute(
                    "INSERT INTO call_record (call_id, trunk_system_id, t_start, encryption, \
                     encryption_evidence, algid, late_entry, reasons) \
                     VALUES (?1, ?2, 1, 'clear', 'algid', 132, 1, '[]')",
                    params![id, sys],
                )
                .is_err(),
            "clear with ALGID 0x84 is a contradiction"
        );

        // The legitimate form does store.
        r.conn
            .execute(
                "INSERT INTO call_record (call_id, trunk_system_id, t_start, encryption, \
                 encryption_evidence, algid, late_entry, reasons) \
                 VALUES (?1, ?2, 1, 'clear', 'algid', 128, 0, '[]')",
                params![id, sys],
            )
            .unwrap();
    }

    /// The read path never invents a state either: a row it cannot make sense of is an error, and
    /// in particular never becomes `clear`.
    #[test]
    fn an_unreadable_encryption_row_is_an_error_not_a_default() {
        assert_eq!(
            enc_from_columns("unknown", None, None, None).unwrap(),
            Encryption::Unknown
        );
        assert!(
            enc_from_columns("clear", None, None, None).is_err(),
            "a claim with no evidence is unreadable, not clear"
        );
        assert!(enc_from_columns("encrypted", None, None, None).is_err());
        assert!(enc_from_columns("unknown", Some("algid".into()), None, None).is_err());
        assert!(enc_from_columns("listenable", None, None, None).is_err());
        assert!(enc_from_columns("clear", Some("vibes".into()), None, None).is_err());
        assert!(
            enc_from_columns("clear", Some("algid".into()), Some(0x84), None).is_err(),
            "clear with an encrypting ALGID"
        );
        assert_eq!(
            enc_from_columns("encrypted", Some("algid".into()), Some(0x84), Some(0x1234)).unwrap(),
            Encryption::Encrypted {
                evidence: EncryptionEvidence::Algid,
                algid: Some(0x84),
                key_id: Some(0x1234),
            }
        );
    }

    #[test]
    fn a_call_seen_encrypted_never_reads_back_clear() {
        let mut r = Repository::open_in_memory().unwrap();
        let s = a_system(&mut r);
        let grant = GrantEvent::new(s.id, GrantKind::Grant, t(1));
        let mut call = CallRecord::from_grant(&grant, false);
        call.encryption = Encryption::from_algid(0x84);
        r.put_call(&call).unwrap();

        // The repository refuses the downgrade...
        let mut downgraded = call.clone();
        downgraded.encryption = Encryption::from_algid(P25_ALGID_CLEAR);
        assert!(r.put_call(&downgraded).is_err());
        downgraded.encryption = Encryption::Unknown;
        assert!(r.put_call(&downgraded).is_err());

        // ...and so does the schema, for anything that reaches it directly.
        assert!(
            r.conn
                .execute(
                    "UPDATE call_record SET encryption = 'unknown', encryption_evidence = NULL, \
                     algid = NULL WHERE call_id = ?1",
                    params![blob(call.id)],
                )
                .is_err()
        );
        assert!(r.call(call.id).unwrap().encryption.is_encrypted());
    }

    #[test]
    fn a_call_fills_in_its_end_and_links_to_the_emission_it_rode_on() {
        let mut r = Repository::open_in_memory().unwrap();
        let s = a_system(&mut r);
        let e = an_emitter(&mut r, 851.2125e6);

        let mut grant = GrantEvent::new(s.id, GrantKind::Grant, t(20));
        grant.talkgroup = Some("101".into());
        grant.slot = Some(1);
        grant.f_hz = Some(851.2125e6);
        grant.encryption = Encryption::from_service_options(false);
        let gid = r.append_grant(&grant).unwrap();
        assert!(gid > 0);

        let mut call = CallRecord::from_grant(&grant, false);
        call.emitter_id = Some(e);
        call.reasons = vec!["grant-followed".into()];
        r.put_call(&call).unwrap();
        assert_eq!(r.open_calls(s.id).unwrap().len(), 1);

        r.close_call(call.id, t(27)).unwrap();
        let back = r.call(call.id).unwrap();
        assert_eq!(back.duration_ns(), Some(7_000_000_000));
        assert_eq!(back.emitter_id, Some(e));
        assert_eq!(back.slot, Some(1));
        assert!(back.encryption.is_clear());
        assert_eq!(
            back.encryption.evidence(),
            Some(EncryptionEvidence::ServiceOptions)
        );
        assert!(r.open_calls(s.id).unwrap().is_empty());
        assert!(
            r.close_call(call.id, t(30)).is_err(),
            "a closed call has no end to fill in"
        );
        assert_eq!(r.calls_for_system(s.id, 10).unwrap(), vec![back]);

        // A call that could not be followed is recorded with its reason, not dropped.
        let mut outside = GrantEvent::new(s.id, GrantKind::OutsideWindow, t(30));
        outside.f_hz = Some(866.0e6);
        r.append_grant(&outside).unwrap();
        let grants = r.grants_for_system(s.id, t(0), 10).unwrap();
        assert_eq!(grants.len(), 2);
        assert_eq!(grants[0].kind, GrantKind::Grant, "oldest first");
        assert_eq!(grants[1].kind, GrantKind::OutsideWindow);
        assert_eq!(grants[1].encryption, Encryption::Unknown);
    }

    #[test]
    fn grant_events_are_append_only() {
        let mut r = Repository::open_in_memory().unwrap();
        let s = a_system(&mut r);
        let mut g = GrantEvent::new(s.id, GrantKind::Grant, t(1));
        g.detail = serde_json::json!({"opcode": "GRP_V_CH_GRANT"});
        r.append_grant(&g).unwrap();
        assert_eq!(r.grants_for_system(s.id, t(0), 10).unwrap()[0], g);

        assert!(
            r.conn
                .execute("UPDATE grant_event SET kind = 'denied'", [])
                .is_err()
        );
        assert!(r.conn.execute("DELETE FROM grant_event", []).is_err());
    }

    #[test]
    fn a_system_round_trips_with_its_tables_and_the_channel_plan_keeps_its_history() {
        let mut r = Repository::open_in_memory().unwrap();
        let mut s = a_system(&mut r);

        let old = ChannelPlanEntry {
            iden: 1,
            base_hz: 851.0e6,
            spacing_hz: 6250.0,
            tx_offset_hz: -45.0e6,
            bandwidth_hz: Some(12_500.0),
            slots: 1,
            t: t(1),
        };
        let new = ChannelPlanEntry {
            base_hz: 851.00625e6,
            t: t(100),
            ..old
        };
        let other = ChannelPlanEntry { iden: 2, ..old };
        for e in [&old, &new, &other] {
            r.append_channel_plan(s.id, e).unwrap();
        }
        let n = NeighbourSite {
            site_id: Some("1:4".into()),
            cc_freq_hz: Some(852.05e6),
            t: t(2),
        };
        r.append_trunk_neighbour(s.id, &n).unwrap();
        let tg = Talkgroup {
            id: "4242".into(),
            label: Some("Fire dispatch".into()),
            label_source: Some(LabelSource::Prior),
            first_seen: t(3),
            last_seen: t(9),
            calls: 5,
        };
        r.upsert_talkgroup(s.id, &tg).unwrap();

        let back = r.trunk_system(s.id).unwrap();
        assert_eq!(back.protocol, TrunkProtocol::P25Phase1);
        assert_eq!(back.system_id.as_deref(), Some("BEE00:2AB"));
        assert_eq!(
            back.channel_plan,
            vec![new, other],
            "the current table is the newest entry per iden"
        );
        assert_eq!(back.neighbours, vec![n]);
        assert_eq!(back.talkgroups, vec![tg.clone()]);
        assert_eq!(back.iden(1), Some(&new));

        // The superseded entry is still readable, which is how a stale table is spotted.
        let history = r.channel_plan_history(s.id, 1, 10).unwrap();
        assert_eq!(history, vec![new, old], "newest first");
        assert!(
            r.conn
                .execute("UPDATE trunk_channel_plan SET base_hz = 1.0", [])
                .is_err(),
            "the channel table is append-only"
        );

        // The talkgroup window widens and its count does not shrink on a replay.
        let replay = Talkgroup {
            first_seen: t(1),
            last_seen: t(4),
            calls: 2,
            ..tg
        };
        r.upsert_talkgroup(s.id, &replay).unwrap();
        let stored = &r.talkgroups(s.id).unwrap()[0];
        assert_eq!(stored.first_seen, t(1));
        assert_eq!(stored.last_seen, t(9));
        assert_eq!(stored.calls, 5);

        // The natural key finds it; an undecoded system is a different row.
        assert_eq!(
            r.find_trunk_system(TrunkProtocol::P25Phase1, Some("BEE00:2AB"), Some("1:3"))
                .unwrap(),
            Some(s.id)
        );
        assert_eq!(
            r.find_trunk_system(TrunkProtocol::P25Phase1, None, None)
                .unwrap(),
            None,
            "not knowing the ids never matches a system that has them"
        );

        s.last_seen = t(500);
        s.updated_at = t(500);
        r.put_trunk_system(&s).unwrap();
        assert_eq!(r.trunk_system(s.id).unwrap().last_seen, t(500));
        assert_eq!(
            r.trunk_systems().unwrap().len(),
            1,
            "updated, not duplicated"
        );
        assert!(
            r.trunk_system_opt(TrunkSystemId::new()).unwrap().is_none(),
            "an unknown id reads as absent, not as an error"
        );
    }

    /// M4 is metadata only. No table here may grow a column that could hold audio, voice frames or
    /// a message payload: that decision is a separate ticket, and while it is unmade the roadmap's
    /// vocoder-IP gate has nothing to bind on.
    #[test]
    fn no_audio_column_exists() {
        let r = Repository::open_in_memory().unwrap();
        let tables = [
            "trunk_system",
            "trunk_channel_plan",
            "trunk_neighbour",
            "trunk_talkgroup",
            "call_record",
            "grant_event",
        ];
        for table in tables {
            let mut stmt = r
                .conn
                .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .unwrap();
            let columns: Vec<String> = stmt
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert!(!columns.is_empty(), "{table} exists");
            for c in &columns {
                for banned in ["audio", "voice", "payload", "samples", "pcm", "vocoder"] {
                    assert!(
                        !c.contains(banned),
                        "{table}.{c} looks like content storage ({banned}); M4 is metadata only"
                    );
                }
            }
        }
        let audio_tables: i64 = r
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' \
                 AND (name LIKE '%audio%' OR name LIKE '%vocoder%')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(audio_tables, 0);
    }
}
