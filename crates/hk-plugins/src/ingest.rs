//! Ingest: plugin output into the Repository, and optionally out on a T-016 messages stream.
//!
//! Persistence goes through `hk_model::Repository`, whose `GatedContent` refusal is the storage
//! gate (T-002). When a decode or annotation carries content under a class that forbids it, the
//! refusal is handled by storing the **metadata-only** form and counting it: the metadata is
//! kept, the content is never written. Republishing sends the stored (possibly metadata-only)
//! row through the egress gate of the publisher, which reduces restricted rows to *its*
//! metadata policy again (a republisher for restricted plugins is created with
//! `Publisher::with_metadata_policy`).
//!
//! **Restricted rows must be sanitised before they get here.** The repository refuses restricted
//! content but stores whatever metadata it is given. Plugin lines are sanitised by
//! [`crate::output::parse_line`]; an in-process producer writing restricted rows must call
//! [`hk_stream::policy::sanitize_decode`] / [`hk_stream::policy::sanitize_annotation`] first.
//! As a fail-closed check, a restricted row that is not allowlist-shaped
//! ([`hk_stream::policy::decode_is_allowlist_shaped`]: nested values, free text, untyped
//! identity) is reduced to the empty allowlist before storage and counted in
//! [`IngestStats::rows_stripped`].
//!
//! **Emitter hook (T-015, T-018).** A stored decode's `identity` (already allowlisted and
//! shape-checked by the time `store` returns it) turns into an inventory sighting built from the
//! returned row, at the INVARIANT comment in [`Ingest::store_decode`]: a point-in-time
//! [`hk_model::Sighting`] at the row's timestamp and the plugin's channel frequency/bandwidth
//! (`None` folds to 0 Hz), carrying the row's content class and the plugin's emitter context.
//! `Repository::record_sighting` (rules in `hk_model::cluster`) resolves it: keyed by the decode
//! id, so re-ingesting the same row never double-counts; the same identity widens its emitter; a
//! context emitter receives the identity unless the scheme shares channels (ADS-B, AIS…).
//! Identity conflicts are reported and counted ([`IngestStats::identity_conflicts`]); failures
//! are counted, never propagated: a plugin's decode stream must not stall because inventory
//! merging disagrees with it.

use std::collections::HashSet;

use hk_model::{Annotation, Decode, EmitterId, ProvenanceId, RepoError, Repository, Sighting};
use hk_stream::policy;
use hk_stream::{MessageRecord, Publisher};

/// Frame model or label kept for a stripped row whose own value is not token-shaped.
const UNSANITIZED: &str = "hackriff.unsanitized/1";

/// Ingest counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IngestStats {
    /// Decode rows written.
    pub decodes_stored: u64,
    /// Annotation rows written.
    pub annotations_stored: u64,
    /// Rows whose content was refused by the gate and stored metadata-only.
    pub content_gated: u64,
    /// Restricted rows that were not allowlist-shaped (skipped the policy) and were reduced to
    /// the empty allowlist before storage.
    pub rows_stripped: u64,
    /// Rows that could not be stored at all.
    pub store_errors: u64,
    /// Rows republished on the stream.
    pub republished: u64,
    /// Republish errors (e.g. oversize message).
    pub republish_errors: u64,
    /// Emitter observations merged for decodes that carried an identity.
    pub emitters_upserted: u64,
    /// Emitter observations that failed to merge (counted, never propagated).
    pub emitter_errors: u64,
    /// Identity sightings that collided with another identity on their cluster (reported by
    /// entity resolution; nothing merged).
    pub identity_conflicts: u64,
}

/// Result of storing one row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stored {
    /// Content was refused and the metadata-only form was stored.
    pub content_gated: bool,
}

/// Recently seen emitters [`Ingest`] remembers, so a re-sighting is not offered again.
pub const MAX_TRACKED_EMITTERS: usize = 4096;

/// Emitters awaiting [`Ingest::take_new_emitters`]. When it is full a new emitter is not marked
/// seen, so its next sighting after a take offers it again: nothing is lost, memory stays bounded.
pub const MAX_PENDING_EMITTERS: usize = 1 << 16;

/// A bounded, approximately least-recently-used set: two generations of at most `cap / 2` ids
/// each. A hit in the old generation is promoted; when the current generation fills, it becomes
/// the old one and the previous old one is forgotten. O(1) per sighting, at most `cap` ids.
#[derive(Default)]
struct RecentEmitters {
    current: HashSet<EmitterId>,
    previous: HashSet<EmitterId>,
}

impl RecentEmitters {
    fn contains_touch(&mut self, id: EmitterId) -> bool {
        if self.current.contains(&id) {
            return true;
        }
        if self.previous.remove(&id) {
            self.insert(id);
            return true;
        }
        false
    }

    fn insert(&mut self, id: EmitterId) {
        if self.current.len() >= MAX_TRACKED_EMITTERS / 2 {
            self.previous = std::mem::take(&mut self.current);
        }
        self.current.insert(id);
    }
}

/// The shared sink for every plugin instance (wrap in `Arc<Mutex<_>>`).
pub struct Ingest {
    repo: Repository,
    republish: Option<Publisher>,
    stats: IngestStats,
    seen: RecentEmitters,
    new_emitters: Vec<EmitterId>,
}

impl Ingest {
    /// Stores into `repo` only.
    pub fn new(repo: Repository) -> Self {
        Self {
            repo,
            republish: None,
            stats: IngestStats::default(),
            seen: RecentEmitters::default(),
            new_emitters: Vec::new(),
        }
    }

    /// The emitters the stored decodes' identity sightings resolved to that were not seen
    /// recently, in first-seen order since the last call, so a caller can add the decoder's service
    /// family to them (T-037b, T-112). A long-running caller drains it periodically: tracking is
    /// bounded (the last [`MAX_TRACKED_EMITTERS`] or so emitters), so an emitter first seen hours
    /// in is still offered, and one silent long enough to be forgotten is offered again (the
    /// family step is idempotent in effect). Ids only: identities stay gated by their decodes'
    /// class.
    pub fn take_new_emitters(&mut self) -> Vec<EmitterId> {
        std::mem::take(&mut self.new_emitters)
    }

    /// Runs `f` with every repository write it makes in one write transaction (T-112), committed
    /// at the end. Each row keeps its own gate, sanitising and error handling: a write that fails
    /// inside is counted as before and does not undo the others (writes that open their own
    /// transaction nest as savepoints). On a failed begin or commit nothing of the batch is
    /// stored and the error is returned.
    pub fn batch<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> Result<T, RepoError> {
        self.repo.begin_write_batch()?;
        let out = f(self);
        self.repo.commit_write_batch()?;
        Ok(out)
    }

    /// Stores into `repo` and republishes each stored row on a messages-stream `publisher`.
    pub fn with_republish(repo: Repository, publisher: Publisher) -> Self {
        Self {
            republish: Some(publisher),
            ..Self::new(repo)
        }
    }

    /// The repository (for queries).
    pub fn repo(&self) -> &Repository {
        &self.repo
    }

    /// Mutable repository access.
    pub fn repo_mut(&mut self) -> &mut Repository {
        &mut self.repo
    }

    /// Counters.
    pub fn stats(&self) -> IngestStats {
        self.stats
    }

    /// Removes the republishing publisher (dropping it finishes the stream).
    pub fn take_publisher(&mut self) -> Option<Publisher> {
        self.republish.take()
    }

    fn store<T>(
        &mut self,
        mut row: T,
        content: fn(&mut T) -> &mut Option<serde_json::Value>,
        insert: fn(&mut Repository, &T) -> Result<(), RepoError>,
    ) -> Result<(T, Stored), RepoError> {
        let mut gated = false;
        match insert(&mut self.repo, &row) {
            Ok(()) => {}
            Err(RepoError::GatedContent { .. }) => {
                *content(&mut row) = None;
                gated = true;
                if let Err(e) = insert(&mut self.repo, &row) {
                    self.stats.store_errors += 1;
                    return Err(e);
                }
                self.stats.content_gated += 1;
            }
            Err(e) => {
                self.stats.store_errors += 1;
                return Err(e);
            }
        }
        Ok((
            row,
            Stored {
                content_gated: gated,
            },
        ))
    }

    fn publish(&mut self, record: MessageRecord) {
        if let Some(p) = &mut self.republish {
            match p.publish_message(&record) {
                Ok(_) => self.stats.republished += 1,
                Err(_) => self.stats.republish_errors += 1,
            }
        }
    }

    /// Stores a decode (metadata-only if its content is gated), upserts an Emitter sighting when
    /// the row carries an identity, and republishes it. A restricted row that is not
    /// allowlist-shaped is reduced to the empty allowlist first. `channel_center_hz`/
    /// `channel_bandwidth_hz` are the plugin's input channel (e.g. the wideband ADS-B window); a
    /// missing value folds to 0 Hz rather than failing the decode.
    pub fn store_decode(
        &mut self,
        mut decode: Decode,
        emitter: Option<EmitterId>,
        provenance: Option<ProvenanceId>,
        channel_center_hz: Option<f64>,
        channel_bandwidth_hz: Option<f64>,
    ) -> Result<Stored, RepoError> {
        if !policy::decode_is_allowlist_shaped(&decode) {
            let fallback = token_or_unsanitized(&decode.frame_model);
            policy::sanitize_decode(None, &fallback, &mut decode);
            self.stats.rows_stripped += 1;
        }
        // INVARIANT (legal guardrail): `row` is the single sanitised decode, the value after the
        // ceiling clamp and metadata/identity allowlist (`parse_line`), the shape check above and
        // the repository content gate (`store`). Every downstream write consumes `row` and nothing
        // earlier: the Decode row, the republish, and any emitter observation (T-015's
        // `upsert_emitter_observation` belongs here, built from `&row`). Nothing may reach another
        // table or stream that the stored Decode row would not hold.
        let (row, stored) = self.store(decode, |d| &mut d.content, Repository::insert_decode)?;
        self.stats.decodes_stored += 1;
        self.upsert_emitter(&row, emitter, channel_center_hz, channel_bandwidth_hz);
        self.publish(MessageRecord::from_decode(&row, emitter, provenance));
        Ok(stored)
    }

    /// One identity sighting for the stored `row` (nothing when it has no identity), resolved by
    /// `Repository::record_sighting` with `context` as the emitter context.
    fn upsert_emitter(
        &mut self,
        row: &Decode,
        context: Option<EmitterId>,
        center_hz: Option<f64>,
        bandwidth_hz: Option<f64>,
    ) {
        let Some(sighting) = Sighting::decode(
            row,
            center_hz.unwrap_or(0.0),
            bandwidth_hz.unwrap_or(0.0),
            context,
        ) else {
            return;
        };
        match self.repo.record_sighting(&sighting, None) {
            Ok(r) => {
                self.stats.emitters_upserted += 1;
                if !self.seen.contains_touch(r.emitter_id)
                    && self.new_emitters.len() < MAX_PENDING_EMITTERS
                {
                    self.seen.insert(r.emitter_id);
                    self.new_emitters.push(r.emitter_id);
                }
                if r.conflict.is_some() {
                    self.stats.identity_conflicts += 1;
                }
            }
            Err(_) => self.stats.emitter_errors += 1,
        }
    }

    /// Stores an annotation (metadata-only if its content is gated) and republishes it. A
    /// restricted row that is not allowlist-shaped is reduced to the empty allowlist first.
    pub fn store_annotation(
        &mut self,
        mut annotation: Annotation,
        emitter: Option<EmitterId>,
        provenance: Option<ProvenanceId>,
    ) -> Result<Stored, RepoError> {
        if !policy::annotation_is_allowlist_shaped(&annotation) {
            let fallback = token_or_unsanitized(&annotation.value);
            policy::sanitize_annotation(None, &fallback, &mut annotation);
            self.stats.rows_stripped += 1;
        }
        let (row, stored) = self.store(
            annotation,
            |a| &mut a.content,
            Repository::insert_annotation,
        )?;
        self.stats.annotations_stored += 1;
        self.publish(MessageRecord::from_annotation(&row, emitter, provenance));
        Ok(stored)
    }
}

fn token_or_unsanitized(value: &str) -> String {
    if policy::is_token(value) {
        value.to_owned()
    } else {
        UNSANITIZED.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::{
        ContentClass, CrcStatus, DecodeId, DecodedIdentity, IdentityAccess, IdentityScheme,
        Timestamp,
    };

    const IDENTITIES: u32 = 5000;

    fn aircraft(icao: u32) -> Decode {
        Decode {
            id: DecodeId::new(),
            demodulation_ref: None,
            recording_ref: None,
            decoder_id: "readsb".into(),
            decoder_version: "1".into(),
            frame_model: "adsb-es".into(),
            metadata: serde_json::json!({"df": 17}),
            content: None,
            crc_status: CrcStatus::Valid,
            identity: Some(DecodedIdentity {
                scheme: IdentityScheme::AdsbIcao,
                value: format!("{icao:06x}"),
            }),
            content_class: ContentClass::Unrestricted,
            t: Timestamp::from_unix_nanos(i64::from(icao) + 1),
            provenance: None,
        }
    }

    fn store(ing: &mut Ingest, icao: u32) {
        ing.store_decode(aircraft(icao), None, None, Some(1090e6), Some(2e6))
            .unwrap();
    }

    /// T-112: a writer that drains periodically (a long ADS-B run) keeps being offered emitters
    /// first seen after the first 4096, while re-sightings of recent ones are not offered again.
    #[test]
    fn a_draining_writer_is_offered_new_emitters_beyond_the_tracking_limit() {
        let mut ing = Ingest::new(Repository::open_in_memory().unwrap());
        let mut offered = HashSet::new();
        for icao in 0..IDENTITIES {
            store(&mut ing, icao);
            // A recent aircraft seen again: no new emitter.
            if icao > 0 {
                store(&mut ing, icao - 1);
            }
            if icao % 500 == 499 {
                for e in ing.take_new_emitters() {
                    assert!(offered.insert(e), "an emitter offered twice while recent");
                }
            }
        }
        offered.extend(ing.take_new_emitters());
        assert!(IDENTITIES as usize > MAX_TRACKED_EMITTERS);
        assert_eq!(offered.len(), IDENTITIES as usize);
        assert_eq!(ing.stats().emitters_upserted, 2 * u64::from(IDENTITIES) - 1);
        assert!(ing.take_new_emitters().is_empty());
        // A late aircraft (past 4096) seen again is still recent: not offered again.
        store(&mut ing, IDENTITIES - 1);
        assert!(ing.take_new_emitters().is_empty());
    }

    /// A plugin chain takes its emitters once at the end: all of them, not the first 4096.
    #[test]
    fn a_chain_that_takes_at_the_end_gets_every_emitter() {
        let mut ing = Ingest::new(Repository::open_in_memory().unwrap());
        for icao in 0..IDENTITIES {
            store(&mut ing, icao);
        }
        let all = ing.take_new_emitters();
        assert_eq!(all.len(), IDENTITIES as usize);
        assert_eq!(all.iter().collect::<HashSet<_>>().len(), all.len());
    }

    /// Rows stored in one batch commit together with their sightings, and a batch leaves the
    /// connection usable for ordinary writes.
    #[test]
    fn a_batch_stores_rows_and_sightings_in_one_transaction() {
        let mut ing = Ingest::new(Repository::open_in_memory().unwrap());
        let ids = ing
            .batch(|ing| {
                let mut ids = Vec::new();
                for icao in 0..10 {
                    let d = aircraft(icao);
                    ids.push(d.id);
                    ing.store_decode(d, None, None, None, None).unwrap();
                }
                ids
            })
            .unwrap();
        assert_eq!(ing.take_new_emitters().len(), 10);
        let s = ing.stats();
        assert_eq!(s.decodes_stored, 10);
        for id in ids {
            ing.repo()
                .decode_with_access(id, IdentityAccess::Standard)
                .unwrap();
        }
        store(&mut ing, 99);
        assert!(ing.repo_mut().commit_write_batch().is_err(), "batch closed");
    }
}
