//! Batched track persistence: upserts of the Track aggregates, then append-only
//! track↔detection links, then merged tracks' links re-pointed to their survivors, then segment
//! boundaries, through the repository. Write the member detections first (e.g. with
//! [`DetectionWriter`](crate::DetectionWriter)): a link whose detection is not stored — never
//! written, or already aged out by detection retention (T-904) — is skipped, not written.
//!
//! One batch is **one** `BEGIN IMMEDIATE` transaction ([`Repository::batch`], T-035): it is
//! written completely or not at all.

use hk_model::{DetectionId, RepoError, Repository, Timestamp, Track, TrackId, TrackSegment};

/// Track rows and links drained from a [`Tracker`](super::Tracker).
#[derive(Debug)]
pub struct TrackBatch {
    /// Aggregates to upsert, in order (merge targets precede the tracks merged into them).
    pub upserts: Vec<Track>,
    /// `(track, detection)` links to append.
    pub links: Vec<(TrackId, DetectionId)>,
    /// `(merged, survivor)`: after the links, the merged track's member links are copied to the
    /// survivor (the link table is append-only, so the merged track keeps its own rows too).
    pub repoints: Vec<(TrackId, TrackId)>,
    /// Segment boundaries crossed by confirmed tracks (T-035).
    pub segments: Vec<TrackSegment>,
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
            repoints: Vec::with_capacity(16),
            segments: Vec::with_capacity(64),
            linked_at: Timestamp::UNIX_EPOCH,
            ids: Vec::with_capacity(256),
        }
    }

    /// Nothing to write.
    pub fn is_empty(&self) -> bool {
        self.upserts.is_empty()
            && self.links.is_empty()
            && self.repoints.is_empty()
            && self.segments.is_empty()
    }

    /// Writes the upserts, then the links (grouped per track), then the re-pointed links of merged
    /// tracks, then the segment boundaries, in one transaction, and clears the batch. Returns
    /// `(tracks upserted, links written)` (re-pointed links included).
    ///
    /// On error nothing is written and the batch is left intact (links may be reordered), so the
    /// caller can retry it.
    pub fn write(&mut self, repo: &mut Repository) -> Result<(usize, usize), RepoError> {
        let Self {
            upserts,
            links,
            repoints,
            segments,
            linked_at,
            ids,
        } = self;
        links.sort_by_key(|&(t, _)| t);
        let written = repo.batch(|tx| {
            for t in upserts.iter() {
                tx.upsert_track(t)?;
            }
            let mut n = links.len();
            let mut i = 0;
            while i < links.len() {
                let track = links[i].0;
                ids.clear();
                while i < links.len() && links[i].0 == track {
                    ids.push(links[i].1);
                    i += 1;
                }
                tx.link_detections_to_track(track, ids, *linked_at)?;
            }
            for &(from, into) in repoints.iter() {
                let moved = tx.track_detections(from)?;
                tx.link_detections_to_track(into, &moved, *linked_at)?;
                n += moved.len();
            }
            tx.append_track_segments(segments)?;
            Ok((upserts.len(), n))
        })?;
        upserts.clear();
        links.clear();
        repoints.clear();
        segments.clear();
        Ok(written)
    }
}
