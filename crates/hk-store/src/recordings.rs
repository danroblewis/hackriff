//! The catalogue of **persisted IQ recordings** (T-469): what SigMF is actually on disk, over
//! what time span, at what tuning, from which device, and where the pipeline can read it.
//!
//! # Why this exists
//!
//! Raw IQ — and therefore audio, demodulation and decode on playback — exists in exactly two
//! places: the rolling IQ ring ([`crate::iqbuffer`], minutes) and **persisted recordings**
//! (`recordings/<id>.sigmf-{meta,data}`, written by the manual recorder, the record chain and the
//! ring's clip export). `GET /api/iqbuffer` already answers the ring's extent exactly. Nothing
//! answered the other half, so the IQ horizon a client could draw was the ring's alone and
//! silently under-reported every recording beyond it. CLAUDE.md's playback invariant requires that
//! horizon to be a *visible, predictable boundary on the time axis*, which a client cannot draw
//! for files it cannot enumerate.
//!
//! # It is a query, not an index
//!
//! Every recording already has an immutable `Recording` row ([`hk_model::Recording`]) carrying its
//! time span, centre, rate, size and provenance, indexed by `t_start`. This module runs
//! [`Repository::recordings_in`] and joins provenance; it maintains nothing of its own. A second
//! catalogue kept beside the rows could only disagree with them.
//!
//! # Honesty: the row is a claim, the file is the fact
//!
//! A `Recording` row is written **after** the samples are, and it is immutable, so it keeps
//! claiming what it claimed when a file is later truncated, evicted or moved to another data
//! directory. A row alone therefore cannot say whether the IQ is there *now*. So every entry is
//! checked against the filesystem once, at list time, and reports an [`Availability`]:
//!
//! - [`Availability::Complete`] — both files exist and the data file is **exactly** the
//!   `size_bytes` the row records. Only these extend the audio horizon.
//! - [`Availability::Partial`] — the files are there but do not match the row: a short data file
//!   (a partially written or truncated recording), one longer than the row records, or a missing
//!   `.sigmf-meta` sidecar without which the samples cannot be interpreted as SigMF.
//! - [`Availability::Missing`] — no data file at all.
//!
//! A partially written recording is therefore listed — hiding it would be its own dishonesty —
//! but never as available, and never in [`RecordingsCatalogue::spans`]. What is *not* covered:
//! a recording still being written has **no row yet** (the recorder inserts one when it stops), so
//! it cannot appear here at all; while it is in progress the ring covers the same samples, and
//! `GET /api/control/state` reports the recorder. Nor is the data file's content verified — the
//! check is existence and length, not a CRC over the samples.

use std::path::{Path, PathBuf};

use hk_model::provenance::Provenance;
use hk_model::recording::{Recording, RecordingKind};
use hk_model::{RepoError, Repository};

/// Most recordings one call lists.
pub const MAX_RECORDINGS_PAGE: usize = 1000;
/// Recordings listed when the caller names no limit.
pub const DEFAULT_RECORDINGS_PAGE: usize = 200;

/// Which recordings to list. Both bounds are open when `None`; the window is half-open, so a
/// recording ending exactly at `t0_ns` does not overlap it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecordingsQuery {
    /// Only recordings ending after this (Unix ns).
    pub t0_ns: Option<i64>,
    /// Only recordings starting before this (Unix ns).
    pub t1_ns: Option<i64>,
    /// Only this kind.
    pub kind: Option<RecordingKind>,
    /// At most this many, newest first.
    pub limit: usize,
}

/// Whether the samples the row claims are on disk **now**.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    /// Both files present and the data file is exactly the length the row records.
    Complete,
    /// Present but not what the row describes: short, long, or no `.sigmf-meta`.
    Partial,
    /// No data file.
    Missing,
}

impl Availability {
    /// The wire/serialised token.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Missing => "missing",
        }
    }

    /// Whether the whole span can be replayed. **Only [`Self::Complete`].**
    pub const fn available(self) -> bool {
        matches!(self, Self::Complete)
    }
}

/// One catalogued recording: its row, the front-end state it was captured under, and what is
/// actually on disk.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordingEntry {
    /// The immutable row.
    pub recording: Recording,
    /// The provenance the row points at — device, tuning, gains, antenna port, bias tee. `None`
    /// when the row's provenance cannot be read (a database written by another run, say); the
    /// recording is still listed, with its tuning from the row itself.
    pub provenance: Option<Provenance>,
    /// What is on disk.
    pub availability: Availability,
    /// Bytes the data file holds; `None` when there is no data file.
    pub bytes_on_disk: Option<u64>,
    /// Whether the `.sigmf-meta` sidecar is there.
    pub meta_present: bool,
    /// Why this is not [`Availability::Complete`]; `None` when it is.
    pub detail: Option<String>,
}

impl RecordingEntry {
    /// Whether this holds raw or channelised IQ (as opposed to demodulated audio), and so can
    /// extend the demod/decode horizon.
    pub const fn is_iq(&self) -> bool {
        matches!(
            self.recording.kind,
            RecordingKind::IqSnippet | RecordingKind::ChannelDecimated
        )
    }

    /// The tuned window, `centre ± rate/2` Hz — the same convention the IQ ring's segments and
    /// the clip band filter use.
    pub fn window_hz(&self) -> (f64, f64) {
        let half = self.recording.sample_rate_hz / 2.0;
        (
            self.recording.f_center_hz - half,
            self.recording.f_center_hz + half,
        )
    }
}

/// One span of IQ that can actually be replayed, Unix ns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableSpan {
    /// First sample.
    pub t0_ns: i64,
    /// End.
    pub t1_ns: i64,
    /// The recording it comes from.
    pub recording: hk_model::RecordingId,
}

/// A page of the catalogue.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordingsCatalogue {
    /// The page, newest first.
    pub entries: Vec<RecordingEntry>,
    /// How many recordings matched the query in total.
    pub matched: u64,
    /// How many matched but were not listed (`matched − entries.len()`).
    pub omitted: u64,
    /// The **complete IQ** spans among [`Self::entries`], oldest first — the part of the audio
    /// horizon these recordings carry. Covers exactly the listed page, never more: a page that
    /// omitted rows omitted their spans too, which is what [`Self::omitted`] is for.
    pub spans: Vec<AvailableSpan>,
}

/// Lists recordings matching `query`, checking each against `data_dir` (the run's data directory,
/// which `Recording::meta_uri`/`data_uri` are relative to).
///
/// The filesystem check is one `metadata` call per file per listed recording, so the cost is the
/// page size, not the catalogue size.
pub fn catalogue(
    repo: &Repository,
    data_dir: &Path,
    query: &RecordingsQuery,
) -> Result<RecordingsCatalogue, RepoError> {
    let limit = query.limit.clamp(1, MAX_RECORDINGS_PAGE);
    let (rows, matched) = repo.recordings_in(query.t0_ns, query.t1_ns, query.kind, limit)?;
    let entries: Vec<RecordingEntry> = rows
        .into_iter()
        .map(|recording| {
            let provenance = repo.provenance(recording.provenance_ref).ok();
            let (availability, bytes_on_disk, meta_present, detail) = check(data_dir, &recording);
            RecordingEntry {
                recording,
                provenance,
                availability,
                bytes_on_disk,
                meta_present,
                detail,
            }
        })
        .collect();
    let mut spans: Vec<AvailableSpan> = entries
        .iter()
        .filter(|e| e.availability.available() && e.is_iq())
        .map(|e| AvailableSpan {
            t0_ns: e.recording.time.start.as_unix_nanos(),
            t1_ns: e.recording.time.end.as_unix_nanos(),
            recording: e.recording.id,
        })
        .collect();
    spans.sort_by_key(|s| (s.t0_ns, s.t1_ns));
    Ok(RecordingsCatalogue {
        omitted: matched.saturating_sub(entries.len() as u64),
        matched,
        entries,
        spans,
    })
}

/// Resolves a recording's `*_uri` against the data directory. A URI is a relative path the writer
/// produced (`recordings/<id>.sigmf-data`); anything absolute or climbing out of the data
/// directory is refused rather than followed.
fn resolve(data_dir: &Path, uri: &str) -> Option<PathBuf> {
    let p = Path::new(uri);
    if p.is_absolute() || p.components().any(|c| c == std::path::Component::ParentDir) {
        return None;
    }
    Some(data_dir.join(p))
}

/// The one filesystem check, per recording.
fn check(data_dir: &Path, rec: &Recording) -> (Availability, Option<u64>, bool, Option<String>) {
    let Some(data) = resolve(data_dir, &rec.data_uri) else {
        return (
            Availability::Missing,
            None,
            false,
            Some("data_uri is not a path inside the data directory".into()),
        );
    };
    let meta_present = resolve(data_dir, &rec.meta_uri).is_some_and(|p| p.is_file());
    let Ok(len) = std::fs::metadata(&data).map(|m| m.len()) else {
        return (
            Availability::Missing,
            None,
            meta_present,
            Some(
                "the .sigmf-data file is not on disk (evicted, moved, or another data directory)"
                    .into(),
            ),
        );
    };
    let expected = rec.size_bytes;
    let detail = if len < expected {
        Some(format!(
            "the .sigmf-data file holds {len} of the {expected} bytes the row records: \
             partially written or truncated"
        ))
    } else if len > expected {
        Some(format!(
            "the .sigmf-data file holds {len} bytes, more than the {expected} the row records: \
             not the samples this row describes"
        ))
    } else if !meta_present {
        Some(
            "the .sigmf-meta sidecar is not on disk, so the samples cannot be read as SigMF".into(),
        )
    } else {
        None
    };
    let state = if detail.is_none() {
        Availability::Complete
    } else {
        Availability::Partial
    };
    (state, Some(len), meta_present, detail)
}

#[cfg(test)]
mod tests;
