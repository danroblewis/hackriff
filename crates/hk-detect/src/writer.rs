//! Batched Detection writes. Provenance is interned by value once per handle
//! ([`Repository::intern_provenance`]), and each record's `provenance_ref` is replaced with the
//! stored id before [`Repository::insert_detections`] writes the batch in one transaction.

use hk_model::{Detection, ProvenanceId, RepoError, Repository};

use crate::record::DetectionRecord;

/// Buffers detections and writes them in batches.
pub struct DetectionWriter {
    batch: Vec<Detection>,
    batch_size: usize,
    interned: Vec<(ProvenanceId, ProvenanceId)>,
    written: u64,
}

impl DetectionWriter {
    /// Writes every `batch_size` detections (at least 1).
    pub fn new(batch_size: usize) -> Self {
        let batch_size = batch_size.max(1);
        Self {
            batch: Vec::with_capacity(batch_size),
            batch_size,
            interned: Vec::new(),
            written: 0,
        }
    }

    /// Buffers one record, writing the batch when full.
    pub fn push(
        &mut self,
        repo: &mut Repository,
        record: &DetectionRecord,
    ) -> Result<(), RepoError> {
        let handle = record.provenance.id();
        let stored = match self.interned.iter().find(|(h, _)| *h == handle) {
            Some(&(_, s)) => s,
            None => {
                let s = repo.intern_provenance(record.provenance.get())?;
                self.interned.push((handle, s));
                s
            }
        };
        let mut d = record.detection.clone();
        d.provenance_ref = stored;
        self.batch.push(d);
        if self.batch.len() >= self.batch_size {
            self.flush(repo)?;
        }
        Ok(())
    }

    /// Writes the buffered detections; returns how many.
    pub fn flush(&mut self, repo: &mut Repository) -> Result<usize, RepoError> {
        if self.batch.is_empty() {
            return Ok(0);
        }
        repo.insert_detections(&self.batch)?;
        let n = self.batch.len();
        self.written += n as u64;
        self.batch.clear();
        Ok(n)
    }

    /// Detections written so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Detections buffered.
    pub fn pending(&self) -> usize {
        self.batch.len()
    }
}
