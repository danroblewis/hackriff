//! Batched track persistence: upserts of the Track aggregates, then append-only
//! track↔detection links, through the repository. Write the member detections first (e.g. with
//! [`DetectionWriter`](crate::DetectionWriter)); the link table references them.

use hk_model::{DetectionId, RepoError, Repository, Timestamp, Track, TrackId};

/// Track rows and links drained from a [`Tracker`](super::Tracker).
#[derive(Debug)]
pub struct TrackBatch {
    /// Aggregates to upsert, in order (merge targets precede the tracks merged into them).
    pub upserts: Vec<Track>,
    /// `(track, detection)` links to append.
    pub links: Vec<(TrackId, DetectionId)>,
    /// Stream time of the drain (`linked_at`).
    pub linked_at: Timestamp,
    ids: Vec<DetectionId>,
}

impl Default for TrackBatch {
    fn default() -> Self {
        Self::new()
    }
}

impl TrackBatch {
    /// An empty batch with room for a typical drain.
    pub fn new() -> Self {
        Self {
            upserts: Vec::with_capacity(64),
            links: Vec::with_capacity(1024),
            linked_at: Timestamp::UNIX_EPOCH,
            ids: Vec::with_capacity(256),
        }
    }

    /// Nothing to write.
    pub fn is_empty(&self) -> bool {
        self.upserts.is_empty() && self.links.is_empty()
    }

    /// Writes the upserts, then the links (grouped per track), and clears the batch. Returns
    /// `(tracks upserted, links written)`.
    pub fn write(&mut self, repo: &mut Repository) -> Result<(usize, usize), RepoError> {
        for t in &self.upserts {
            repo.upsert_track(t)?;
        }
        let tracks = self.upserts.len();
        let links = self.links.len();
        self.links.sort_by_key(|&(t, _)| t);
        let mut i = 0;
        while i < self.links.len() {
            let track = self.links[i].0;
            self.ids.clear();
            while i < self.links.len() && self.links[i].0 == track {
                self.ids.push(self.links[i].1);
                i += 1;
            }
            repo.link_detections_to_track(track, &self.ids, self.linked_at)?;
        }
        self.upserts.clear();
        self.links.clear();
        Ok((tracks, links))
    }
}
