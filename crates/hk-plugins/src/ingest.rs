//! Ingest: plugin output into the Repository, and optionally out on a T-016 messages stream.
//!
//! Persistence goes through `hk_model::Repository`, whose `GatedContent` refusal is the storage
//! gate (T-002). When a decode or annotation carries content under a class that forbids it, the
//! refusal is handled by storing the **metadata-only** form and counting it: the metadata is
//! kept, the content is never written. Republishing sends the stored (possibly metadata-only)
//! row through the egress gate of the publisher.

use hk_model::{Annotation, Decode, EmitterId, ProvenanceId, RepoError, Repository};
use hk_stream::{MessageRecord, Publisher};

/// Ingest counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IngestStats {
    /// Decode rows written.
    pub decodes_stored: u64,
    /// Annotation rows written.
    pub annotations_stored: u64,
    /// Rows whose content was refused by the gate and stored metadata-only.
    pub content_gated: u64,
    /// Rows that could not be stored at all.
    pub store_errors: u64,
    /// Rows republished on the stream.
    pub republished: u64,
    /// Republish errors (e.g. oversize message).
    pub republish_errors: u64,
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

    /// Stores a decode (metadata-only if its content is gated) and republishes it.
    pub fn store_decode(
        &mut self,
        decode: Decode,
        emitter: Option<EmitterId>,
        provenance: Option<ProvenanceId>,
    ) -> Result<Stored, RepoError> {
        let (row, stored) = self.store(decode, |d| &mut d.content, Repository::insert_decode)?;
        self.stats.decodes_stored += 1;
        self.publish(MessageRecord::from_decode(&row, emitter, provenance));
        Ok(stored)
    }

    /// Stores an annotation (metadata-only if its content is gated) and republishes it.
    pub fn store_annotation(
        &mut self,
        annotation: Annotation,
        emitter: Option<EmitterId>,
        provenance: Option<ProvenanceId>,
    ) -> Result<Stored, RepoError> {
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
