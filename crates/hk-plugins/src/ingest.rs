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
//! **Emitter hook (T-015).** A stored decode's `identity` (already allowlisted and shape-checked
//! by the time `store` returns it) turns into an inventory sighting: a stored decode with an
//! identity upserts one [`hk_model::EmitterObservation`] built from the returned row, at the
//! INVARIANT comment in [`Ingest::store_decode`] — a point-in-time sighting at the row's
//! timestamp and the plugin's channel frequency/bandwidth (whichever channel it was fed; `None`
//! folds to 0 Hz). `Repository::upsert_emitter_observation` (docs/07 §2.11) does the merge: same
//! identity widens the existing emitter's `last_seen`, a new identity creates one. Failures (e.g.
//! an identity now held by a different emitter than a stale caller expected) are counted, never
//! propagated: a plugin's decode stream must not stall because inventory merging disagrees with
//! it.

use hk_model::{
    Annotation, Decode, DecodedIdentity, EmitterId, EmitterObservation, ProvenanceId, RepoError,
    Repository, TimeRange, Timestamp,
};
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
}

/// Result of storing one row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stored {
    /// Content was refused and the metadata-only form was stored.
    pub content_gated: bool,
}

/// The shared sink for every plugin instance (wrap in `Arc<Mutex<_>>`).
pub struct Ingest {
    repo: Repository,
    republish: Option<Publisher>,
    stats: IngestStats,
}

impl Ingest {
    /// Stores into `repo` only.
    pub fn new(repo: Repository) -> Self {
        Self {
            repo,
            republish: None,
            stats: IngestStats::default(),
        }
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
        if let Some(identity) = &row.identity {
            self.upsert_emitter(identity, row.t, channel_center_hz, channel_bandwidth_hz);
        }
        self.publish(MessageRecord::from_decode(&row, emitter, provenance));
        Ok(stored)
    }

    /// One sighting at `t` for `identity`. A fresh candidate id is minted each call: the upsert
    /// only uses it when no emitter already holds the identity, so repeated sightings of the same
    /// identity always merge into one emitter rather than colliding.
    fn upsert_emitter(
        &mut self,
        identity: &DecodedIdentity,
        t: Timestamp,
        center_hz: Option<f64>,
        bandwidth_hz: Option<f64>,
    ) {
        let obs = EmitterObservation {
            emitter_id: EmitterId::new(),
            seen: TimeRange::instant(t),
            count: 1,
            f_center_hz: center_hz.unwrap_or(0.0),
            bandwidth_hz: bandwidth_hz.unwrap_or(0.0),
            identity: Some(identity.clone()),
        };
        match self.repo.upsert_emitter_observation(&obs) {
            Ok(_) => self.stats.emitters_upserted += 1,
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
