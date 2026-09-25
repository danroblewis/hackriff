//! T-904: retention for per-frame [`crate::Detection`] rows (docs/07 §2.9).
//!
//! **Why.** The detector writes one row per detected region per STFT frame. Measured on the
//! staging device on 2026-09-24 (2.4 Msps, live HackRF) that was ~65–90 rows/s at ~530 B/row with
//! its four indexes and its `track_detection` link: ~125 MB/h, ~3 GB/day — and nothing ever
//! deleted a row. On a portable handheld that fills the device.
//!
//! **The policy: age out, rolled up, with a protected tail.** Chosen over plain age-out because
//! of what the served queries read; every reader of `detection` falls in one of three classes:
//!
//! 1. **An emitter's newest-N linked detections** — `/api/inventory`'s `snr_db`/`peak_dbfs`/
//!    `measured` (newest 1), the overlap re-analysis's measured bands and evidence (newest
//!    [`super::relate::MAX_REGION_BANDS`] / [`super::relate::MAX_EVIDENCE_DETECTIONS`]) and the
//!    cross-centre retune test (newest [`super::retune::MAX_RETUNE_DETECTIONS`]). These rank by
//!    `t_start` over a UNION ALL of the rows reached through the emitter's currently-linked
//!    tracks and its direct detection links. A row among the newest [`KEEP_PER_EMITTER`] of *any*
//!    emitter that currently links one of its tracks is **never pruned**, so each of those
//!    queries answers exactly as it did before the prune — not approximately.
//! 2. **A row referenced by id** — decode provenance (`demodulation.detection_id`), a recording's
//!    trigger, a retune verdict, a direct emitter link, an annotation, an anomaly subject, a
//!    classification's input, an observation-ledger source. These are **pinned**: never pruned.
//! 3. **A time-windowed region read** — occupancy (`/api/occupancy` spans, the channel plan).
//!    Past the retention age it reads the [`DetectionRollup`]s instead: one row per contiguous run
//!    of one track's detections (same survey and provenance, no gap over
//!    [`DetectionRetention::rollup_gap_ns`], no longer than
//!    [`DetectionRetention::rollup_span_ns`]) carrying the time hull, the frequency envelope,
//!    means/maxima, the count and the flags. Coarser than the rows, and honest about it: a rollup
//!    is a summary, never presented as a detection. A rollup covers only rows that were
//!    **deleted**: a row the pass kept closes the run it falls in (T-913 [`Barriers`]), so the two
//!    never count the same air twice — nor does a rollup itself, whose `on_air_ns` is the union of
//!    its members' intervals, not their sum.
//!
//!    The multipath relation (`hk-context::multipath`) is **not** in this class: it reads
//!    per-frame rows only inside its own window (60 s by default, far inside the run's
//!    `MIN_RETENTION_S` floor of 10 min) and never reads rollups — a rollup has no per-frame
//!    arrival time, which is the whole of what it measures. If that window is ever widened past
//!    the retention age it will simply see fewer rows, never rollups.
//!
//! Tracks, emitters, presence intervals, the observation ledger and every link are **durable**
//! and untouched: they are the history catalogue (workflow #3), and a one-off burst stays a track
//! with its rollup whatever happens to its per-frame rows.
//!
//! **Time basis.** The age is measured from a **per-survey** watermark (the store's own capture
//! clock, like the history pyramid's watermark), never the wall clock: a replayed recording from
//! last year is not "a year old", and a prune is deterministic under test. An **open** survey
//! ages from its own newest `t_end`, so a replay into a data directory holding newer rows, a host
//! clock behind the store (a Jetson with no RTC) or another survey's future-stamped row can never
//! age out rows its running tracker still holds tentative links to; a closed survey ages from the
//! store's newest row. The composed daemons also floor the age well above the tracker's hold
//! windows (`hk-pipeline` `retention::MIN_RETENTION_S`), and a link whose detection is gone is
//! skipped rather than failing its batch (`link_detections_on`), so no clock anomaly can wedge
//! track persistence.
//!
//! **Rollup grouping.** A tracked row rolls up with its track's contiguous run. A row no track
//! links rolls up only with rows of the same survey and provenance that **overlap it in
//! frequency** (envelope ≤ 2× the widest member) within the gap and span — so an isolated
//! one-off burst keeps its own time–frequency box as its own rollup, and bursts at different
//! frequencies are never merged into one box spanning the window.
//!
//! **Cost (T-453).** A pass never holds the write lock for more than one batch
//! ([`DetectionRetention::batch`] rows): candidates, their tracks and the per-emitter tail cut are
//! read **before** the lock is taken; inside it only the pin checks (all indexed), the rollup
//! upserts and the deletes run. The caller's `between` hook runs between batches (the pipeline
//! sleeps there so ingest gets the lock). [`PruneReport::lock_ns_max`] is the measured worst hold.
//!
//! **Races.** The tail cut is computed from a snapshot. Adding rows to an emitter can only move
//! its K-th newest `t_start` later, so a stale cut protects a superset — safe. The one change that
//! can *shrink* protection is a new link onto a track (a new emitter over an old track, or a merge
//! re-pointing links to the survivor): under the write lock every candidate's tracks are
//! re-checked — the track set of the row, and the emitter set of each track, against what the cut
//! was derived from. A row whose tracks moved is kept this batch and the moved track's cut is
//! re-derived for the next one. The check is per track and indexed, so a live run that links a
//! new track every few seconds costs nothing extra.

use std::collections::HashMap;
use std::time::Instant;

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::{RepoError, Repository, blob, bump_extent, region_bounds};
use crate::detection::DetectionFlags;
use crate::ids::{ProvenanceId, SurveyId, TrackId};
use crate::region::{FreqRange, Region, TimeRange};
use crate::time::Timestamp;

/// Detections kept per emitter, newest first, whatever their age: the largest row cap of any
/// per-emitter "newest linked detections" query (see the module docs, class 1).
pub const KEEP_PER_EMITTER: usize = 256;

const _: () = assert!(KEEP_PER_EMITTER >= super::relate::MAX_EVIDENCE_DETECTIONS);
const _: () = assert!(KEEP_PER_EMITTER >= super::relate::MAX_REGION_BANDS);
const _: () = assert!(KEEP_PER_EMITTER >= super::retune::MAX_RETUNE_DETECTIONS);

const NS_PER_S: i64 = 1_000_000_000;

/// The retention policy for per-frame detection rows (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DetectionRetention {
    /// A row is eligible once its `t_end` is older than its survey's watermark minus this, ns
    /// ([`Repository::prune_detections`]).
    pub max_age_ns: i64,
    /// Fold each pruned row into a [`DetectionRollup`] first. Off = plain age-out (the region
    /// reads then see nothing past the age).
    pub rollup: bool,
    /// Rows kept per linked emitter, newest first ([`KEEP_PER_EMITTER`]; smaller only in tests —
    /// below it the class-1 queries are no longer exact).
    pub keep_per_emitter: usize,
    /// Candidate rows per write transaction.
    pub batch: usize,
    /// Longest silence inside one rollup, ns. The tracker already decided the members are one
    /// signal; this only keeps separate bursts separate (on the staging device 1.2 % of
    /// consecutive gaps inside a track exceeded 10 s, 15 % exceeded 1 s).
    pub rollup_gap_ns: i64,
    /// Longest time hull of one rollup, ns.
    pub rollup_span_ns: i64,
}

impl Default for DetectionRetention {
    /// One hour, rolled up, the full tail, 100-row batches, 10 s gaps, 60 s rollups.
    ///
    /// **Why an hour.** No reader needs per-frame rows older than that: the per-emitter queries
    /// read the protected tail, the IQ ring (the only thing a detection id is replayed against)
    /// holds minutes, and occupancy closes read since their last close (15 min) — past the age
    /// they read rollups. At the staging device's 88 rows/s × ~590 B an hour of rows is ~190 MB;
    /// a day would be ~4.6 GB, which a handheld cannot spare. Configurable
    /// (`HK_DETECTION_RETENTION`).
    ///
    /// The batch is what bounds a write-lock hold: 500 rows held it 310 ms (debug build, host
    /// load ~30) in the T-904 mock-SDR measurement, 100 keeps it an order of magnitude shorter.
    fn default() -> Self {
        Self {
            max_age_ns: 3_600 * NS_PER_S,
            rollup: true,
            keep_per_emitter: KEEP_PER_EMITTER,
            batch: 100,
            rollup_gap_ns: 10 * NS_PER_S,
            rollup_span_ns: 60 * NS_PER_S,
        }
    }
}

/// What one [`Repository::prune_detections`] pass did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PruneReport {
    /// The newest survey watermark of the pass (`None`: no detections). Each survey ages from its
    /// own ([`Repository::prune_detections`]).
    pub watermark: Option<Timestamp>,
    /// [`Self::watermark`] minus the age: that survey's rows ending before this were eligible.
    pub cutoff: Option<Timestamp>,
    /// Eligible rows looked at.
    pub examined: u64,
    /// Rows deleted (with their `track_detection` links).
    pub deleted: u64,
    /// Eligible rows kept because something references them by id (class 2).
    pub kept_pinned: u64,
    /// Eligible rows kept as some emitter's newest [`DetectionRetention::keep_per_emitter`].
    pub kept_tail: u64,
    /// Eligible rows kept because their tracks, or the emitters linking them, changed between
    /// the read and the write lock (re-derived next batch).
    pub kept_moved: u64,
    /// Rollup rows created.
    pub rollups_inserted: u64,
    /// Rollup rows extended (a later batch continuing a track's newest rollup).
    pub rollups_extended: u64,
    /// Write transactions taken.
    pub batches: u64,
    /// Longest write-lock hold of one batch, ns (measured, T-453): from the lock being granted
    /// to the commit, excluding the wait for it ([`Self::wait_ns_max`]) and the WAL checkpoint
    /// that follows the commit (run after the lock is released).
    pub lock_ns_max: u64,
    /// Longest wait for the write lock before one batch, ns: time spent behind the detector's
    /// own writes, which the pass pays and ingest does not.
    pub wait_ns_max: u64,
    /// Total write-lock hold, ns.
    pub lock_ns_total: u64,
    /// Tracks whose cached tail cut was dropped because an emitter link onto them appeared.
    pub relinks: u64,
    /// The pass reached the end of the eligible rows (false: `between` stopped it).
    pub complete: bool,
}

/// How big the detection store is ([`Repository::detection_storage`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DetectionStorage {
    /// Database file bytes (`page_count × page_size`).
    pub db_bytes: u64,
    /// Of which on the free list — reused by later inserts; SQLite does not shrink the file.
    pub free_bytes: u64,
    /// The `-wal` file's bytes (`None`: in-memory database or no WAL file).
    pub wal_bytes: Option<u64>,
    /// Detection rows (0 when [`Self::rows_counted`] is false).
    pub detection_rows: u64,
    /// Rollup rows (0 when [`Self::rows_counted`] is false).
    pub rollup_rows: u64,
    /// Whether the two row counts were actually counted on this call (T-913: a `count(*)` is a
    /// full scan of the smallest index and holds a read snapshot for its duration, so the run's
    /// status refresh does not pay it every time).
    pub rows_counted: bool,
    /// The oldest-ending stored detection's extent (the key the policy ages by).
    pub oldest_detection: Option<TimeRange>,
    /// The newest stored `t_end` (the policy's watermark).
    pub newest_detection_end: Option<Timestamp>,
}

/// A contiguous run of one track's pruned detections (the migration `0019` table).
#[derive(Clone, Debug, PartialEq)]
pub struct DetectionRollup {
    /// Row id.
    pub id: i64,
    /// The track the detections were linked to (`None`: never linked).
    pub track_id: Option<TrackId>,
    /// Survey all of them belong to.
    pub survey_id: SurveyId,
    /// Provenance all of them were measured under.
    pub provenance_ref: ProvenanceId,
    /// Time hull — **not** the time on air (that is [`Self::on_air_ns`]).
    pub time: TimeRange,
    /// Time on air inside the hull, ns: the **union** of the members' intervals (T-913), never
    /// their sum — co-timed members (an FSK signal's two lobes in one frame) are one span of air,
    /// and the figure never exceeds `t_end - t_start`.
    pub on_air_ns: i64,
    /// Frequency envelope (min `f_lo` .. max `f_hi`).
    pub freq: FreqRange,
    /// Mean centre, Hz.
    pub f_center_mean_hz: f64,
    /// Mean OBW, Hz.
    pub obw_mean_hz: f64,
    /// Widest OBW, Hz.
    pub obw_max_hz: f64,
    /// Highest peak SNR, dB.
    pub snr_peak_max_db: f64,
    /// Mean of the members' mean SNR, dB.
    pub snr_mean_db: f64,
    /// Highest peak level, dBFS.
    pub peak_level_dbfs_max: f64,
    /// Detections folded in.
    pub detections: u64,
    /// Flags set on any member (bitmask; `spur_reason` is not kept).
    pub flags_any: DetectionFlags,
    /// Flags set on every member.
    pub flags_all: DetectionFlags,
    /// Summed clip count.
    pub clip_count: u64,
}

const ROLLUP_COLUMNS: &str = "rollup_id, track_id, survey_id, provenance_id, t_start, t_end, \
     on_air_ns, f_lo, f_hi, f_center_mean, obw_mean, obw_max, snr_peak_max, snr_mean_mean, \
     peak_dbfs_max, detections, flags_any, flags_all, clip_count";

fn rollup_from_row(r: &Row<'_>) -> rusqlite::Result<DetectionRollup> {
    let id = |b: [u8; 16]| uuid::Uuid::from_bytes(b);
    Ok(DetectionRollup {
        id: r.get(0)?,
        track_id: r
            .get::<_, Option<[u8; 16]>>(1)?
            .map(|b| TrackId::from_uuid(id(b))),
        survey_id: SurveyId::from_uuid(id(r.get(2)?)),
        provenance_ref: ProvenanceId::from_uuid(id(r.get(3)?)),
        time: TimeRange::new(
            Timestamp::from_unix_nanos(r.get(4)?),
            Timestamp::from_unix_nanos(r.get(5)?),
        ),
        on_air_ns: r.get(6)?,
        freq: FreqRange::new(r.get(7)?, r.get(8)?),
        f_center_mean_hz: r.get(9)?,
        obw_mean_hz: r.get(10)?,
        obw_max_hz: r.get(11)?,
        snr_peak_max_db: r.get(12)?,
        snr_mean_db: r.get(13)?,
        peak_level_dbfs_max: r.get(14)?,
        detections: r.get::<_, i64>(15)?.max(0) as u64,
        flags_any: DetectionFlags::from_bits(r.get::<_, i64>(16)? as u32),
        flags_all: DetectionFlags::from_bits(r.get::<_, i64>(17)? as u32),
        clip_count: r.get::<_, i64>(18)?.max(0) as u64,
    })
}

/// One eligible row, read before the write lock.
struct Candidate {
    id: [u8; 16],
    survey: [u8; 16],
    provenance: [u8; 16],
    t_start: i64,
    t_end: i64,
    f_center: f64,
    obw: f64,
    f_lo: f64,
    f_hi: f64,
    snr_peak: f64,
    snr_mean: f64,
    flags: i64,
    peak_dbfs: f64,
    clip_count: i64,
    /// Its tracks, sorted (a re-pointed row is in two).
    tracks: Vec<[u8; 16]>,
}

const CANDIDATES_SQL: &str = "\
     SELECT detection_id, survey_id, provenance_id, t_start, t_end, f_center, obw, f_lo, f_hi, \
            snr_peak, snr_mean, flags, peak_dbfs, clip_count \
     FROM detection \
     WHERE survey_id = ?5 AND t_end < ?1 AND t_end >= ?2 AND (t_end, detection_id) > (?2, ?3) \
     ORDER BY t_end, detection_id LIMIT ?4";

const TRACKS_OF_SQL: &str =
    "SELECT track_id FROM track_detection WHERE detection_id = ?1 ORDER BY track_id";

const EMITTERS_OF_TRACK_SQL: &str = "SELECT emitter_id FROM emitter_link \
     WHERE target_kind = 'track' AND target_id = ?1 AND superseded_by IS NULL";

/// The K-th newest `t_start` over exactly the rows the class-1 queries rank (`?1` emitter, `?2`
/// = K − 1). No row: the emitter has fewer than K, so all of them are protected.
const EMITTER_TAIL_CUT_SQL: &str = "\
     SELECT t_start FROM ( \
       SELECT d.t_start AS t_start \
       FROM emitter_link el \
       JOIN track_detection td ON td.track_id = el.target_id \
       JOIN detection d ON d.detection_id = td.detection_id \
       WHERE el.emitter_id = ?1 AND el.target_kind = 'track' AND el.superseded_by IS NULL \
       UNION ALL \
       SELECT d.t_start AS t_start \
       FROM emitter_link el \
       JOIN detection d ON d.detection_id = el.target_id \
       WHERE el.emitter_id = ?1 AND el.target_kind = 'detection' AND el.superseded_by IS NULL \
     ) ORDER BY t_start DESC LIMIT 1 OFFSET ?2";

/// Anything that names this detection by id (class 2). Every lookup is indexed — including an
/// explanation's [`crate::Evidence::Detection`], which lives in an opaque JSON body and is written
/// out to `explanation_detection` for exactly this check (T-913, migration 0020).
const PINNED_SQL: &str = "\
     SELECT EXISTS (SELECT 1 FROM emitter_link WHERE target_kind = 'detection' AND target_id = ?1) \
         OR EXISTS (SELECT 1 FROM recording WHERE trigger_detection_id = ?1) \
         OR EXISTS (SELECT 1 FROM demodulation WHERE detection_id = ?1) \
         OR EXISTS (SELECT 1 FROM detection_retune WHERE detection_id = ?1) \
         OR EXISTS (SELECT 1 FROM annotation WHERE target_kind = 'detection' AND target_id = ?1) \
         OR EXISTS (SELECT 1 FROM anomaly WHERE subject_kind = 'detection' AND subject_id = ?1) \
         OR EXISTS (SELECT 1 FROM emitter_classification \
                    WHERE input_kind = 'detection' AND input_id = ?1) \
         OR EXISTS (SELECT 1 FROM emitter_observation \
                    WHERE source_kind = 'detection' AND source_id = ?1) \
         OR EXISTS (SELECT 1 FROM explanation_detection WHERE detection_id = ?1)";

/// The rollup a track's next pruned row may extend: its newest.
const LAST_ROLLUP_SQL: &str = "\
     SELECT rollup_id, track_id, survey_id, provenance_id, t_start, t_end, on_air_ns, \
            f_lo, f_hi, f_center_mean, obw_mean, obw_max, snr_peak_max, snr_mean_mean, \
            peak_dbfs_max, detections, flags_any, flags_all, clip_count \
     FROM detection_rollup WHERE track_id IS ?1 ORDER BY t_start DESC, rollup_id DESC LIMIT 1";

/// The untracked rollup an untracked pruned row may extend (`?1` survey, `?2` provenance, `?3`
/// earliest `t_start` that can still be within the gap and span, `?4`/`?5` the row's `f_hi`/`f_lo`):
/// the newest that overlaps it in frequency. [`Acc::same_emission`] decides the rest.
const UNTRACKED_ROLLUP_SQL: &str = "\
     SELECT rollup_id, track_id, survey_id, provenance_id, t_start, t_end, on_air_ns, \
            f_lo, f_hi, f_center_mean, obw_mean, obw_max, snr_peak_max, snr_mean_mean, \
            peak_dbfs_max, detections, flags_any, flags_all, clip_count \
     FROM detection_rollup \
     WHERE track_id IS NULL AND t_start >= ?3 AND survey_id = ?1 AND provenance_id = ?2 \
       AND f_lo <= ?4 AND f_hi >= ?5 \
     ORDER BY t_start DESC, rollup_id DESC LIMIT 1";

/// T-913: the **kept** rows a rollup must not span. A rollup summarises rows that were
/// *deleted*; a row that survived inside its hull would be counted twice by a region read (once
/// as itself, once inside the summary), contradicting "never count the same air twice". So a kept
/// row is a barrier: the run it falls in is closed, and the next pruned row of that track — or,
/// untracked, of that emission — starts a new rollup after it.
///
/// Kept in memory for the pass (candidates arrive in `t_end` order, so a barrier is seen before
/// any row that could span it) and bounded: only the newest barrier per track is needed, and the
/// untracked list is capped at [`MAX_UNTRACKED_BARRIERS`] — dropping the oldest can only restore
/// the old, harmless over-count, never delete a row that should have been kept. A rollup stored
/// by an *earlier pass* is checked against the table itself ([`survives_between`]).
#[derive(Default)]
struct Barriers {
    /// Track → its newest kept row's `t_end`.
    tracked: HashMap<[u8; 16], i64>,
    /// Kept rows no track links: `(survey, provenance, f_lo, f_hi, t_end)`, oldest first.
    untracked: Vec<([u8; 16], [u8; 16], f64, f64, i64)>,
}

/// How many untracked barriers one pass remembers.
const MAX_UNTRACKED_BARRIERS: usize = 512;

impl Barriers {
    /// Records a row the pass kept.
    fn note(&mut self, c: &Candidate) {
        if c.tracks.is_empty() {
            if self.untracked.len() >= MAX_UNTRACKED_BARRIERS {
                self.untracked.remove(0);
            }
            self.untracked
                .push((c.survey, c.provenance, c.f_lo, c.f_hi, c.t_end));
            return;
        }
        for &t in &c.tracks {
            let e = self.tracked.entry(t).or_insert(c.t_end);
            *e = (*e).max(c.t_end);
        }
    }

    /// Whether folding `c` into `acc` would put a kept row inside the resulting hull.
    fn blocks(&self, acc: &Acc, c: &Candidate) -> bool {
        let lo = acc.t_start.min(c.t_start);
        let hi = acc.t_end.max(c.t_end);
        // Strictly inside: a barrier at the very edge of the hull is not *within* it — the row
        // after a kept row starts where the kept row ended, and must be free to open a new run.
        let inside = |t: i64| t > lo && t < hi;
        match acc.track {
            Some(track) => self.tracked.get(&track).is_some_and(|&t| inside(t)),
            None => self.untracked.iter().any(|&(s, p, f_lo, f_hi, t)| {
                s == acc.survey
                    && p == acc.provenance
                    && f_lo <= acc.f_hi.max(c.f_hi)
                    && f_hi >= acc.f_lo.min(c.f_lo)
                    && inside(t)
            }),
        }
    }
}

/// Whether any `detection` row of `survey` survives between two times (inclusive of both) — the cross-pass
/// form of a [`Barriers`] check, run once per stored rollup rather than per row. Every row older
/// than the candidate being placed has already been examined by this or an earlier pass, so a row
/// still in the table there is one that was kept.
fn survives_between(
    conn: &Connection,
    survey: [u8; 16],
    after: i64,
    before: i64,
) -> Result<bool, RepoError> {
    if before <= after {
        return Ok(false);
    }
    Ok(conn
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM detection \
             WHERE survey_id = ?1 AND t_end >= ?2 AND t_end <= ?3)",
        )?
        .query_row(params![survey, after, before], |r| r.get(0))?)
}

/// The rollups one batch builds or extends: per track, and — for rows no track links (T-075
/// short bursts stored untracked, members of tentative tracks that never confirmed) — per
/// emission, by frequency ([`Acc::same_emission`]), never one bucket for the whole window.
#[derive(Default)]
struct Rollups {
    tracked: HashMap<[u8; 16], Acc>,
    untracked: Vec<Acc>,
}

impl Rollups {
    fn add(
        &mut self,
        conn: &Connection,
        c: &Candidate,
        p: &DetectionRetention,
        barriers: &Barriers,
        report: &mut PruneReport,
    ) -> Result<(), RepoError> {
        let Some(&track) = c.tracks.first() else {
            return self.add_untracked(conn, c, p, barriers);
        };
        if let std::collections::hash_map::Entry::Vacant(slot) = self.tracked.entry(track) {
            let stored = conn
                .prepare_cached(LAST_ROLLUP_SQL)?
                .query_row([track], Acc::stored)
                .optional()?;
            // T-913: not across a row an earlier pass kept.
            let stored = match stored {
                Some(st) if survives_between(conn, c.survey, st.t_end, c.t_start)? => None,
                other => other,
            };
            if let Some(s) = stored {
                slot.insert(s);
            }
        }
        match self.tracked.get_mut(&track) {
            Some(acc) if acc.continues(c, p) && !barriers.blocks(acc, c) => acc.add(c),
            Some(acc) => {
                acc.flush_counted(conn, report)?;
                *acc = Acc::of(Some(track), c);
            }
            None => {
                self.tracked.insert(track, Acc::of(Some(track), c));
            }
        }
        Ok(())
    }

    fn add_untracked(
        &mut self,
        conn: &Connection,
        c: &Candidate,
        p: &DetectionRetention,
        barriers: &Barriers,
    ) -> Result<(), RepoError> {
        let fits = |a: &Acc| a.continues(c, p) && a.same_emission(c) && !barriers.blocks(a, c);
        if let Some(acc) = self.untracked.iter_mut().rev().find(|a| fits(a)) {
            acc.add(c);
            return Ok(());
        }
        let earliest = c
            .t_start
            .saturating_sub(p.rollup_span_ns)
            .saturating_sub(p.rollup_gap_ns);
        let stored = conn
            .prepare_cached(UNTRACKED_ROLLUP_SQL)?
            .query_row(
                params![c.survey, c.provenance, earliest, c.f_hi, c.f_lo],
                Acc::stored,
            )
            .optional()?
            .filter(|s| fits(s) && !self.untracked.iter().any(|a| a.id == s.id));
        // T-913: not across a row an earlier pass kept.
        let stored = match stored {
            Some(st) if survives_between(conn, c.survey, st.t_end, c.t_start)? => None,
            other => other,
        };
        let mut acc = match stored {
            Some(mut s) => {
                s.add(c);
                s
            }
            None => Acc::of(None, c),
        };
        acc.dirty = true;
        self.untracked.push(acc);
        Ok(())
    }

    fn flush(&mut self, conn: &Connection, report: &mut PruneReport) -> Result<(), RepoError> {
        for acc in self.tracked.values_mut().chain(self.untracked.iter_mut()) {
            acc.flush_counted(conn, report)?;
        }
        Ok(())
    }
}

/// A rollup being built or extended inside one batch.
#[derive(Clone, Debug)]
struct Acc {
    id: Option<i64>,
    track: Option<[u8; 16]>,
    survey: [u8; 16],
    provenance: [u8; 16],
    t_start: i64,
    t_end: i64,
    on_air_ns: i64,
    f_lo: f64,
    f_hi: f64,
    f_center_mean: f64,
    obw_mean: f64,
    obw_max: f64,
    snr_peak_max: f64,
    snr_mean_mean: f64,
    peak_dbfs_max: f64,
    n: i64,
    flags_any: i64,
    flags_all: i64,
    clip_count: i64,
    dirty: bool,
}

impl Acc {
    fn of(track: Option<[u8; 16]>, c: &Candidate) -> Self {
        Self {
            id: None,
            track,
            survey: c.survey,
            provenance: c.provenance,
            t_start: c.t_start,
            t_end: c.t_end,
            on_air_ns: c.t_end - c.t_start,
            f_lo: c.f_lo,
            f_hi: c.f_hi,
            f_center_mean: c.f_center,
            obw_mean: c.obw,
            obw_max: c.obw,
            snr_peak_max: c.snr_peak,
            snr_mean_mean: c.snr_mean,
            peak_dbfs_max: c.peak_dbfs,
            n: 1,
            flags_any: c.flags,
            flags_all: c.flags,
            clip_count: c.clip_count,
            dirty: true,
        }
    }

    fn stored(r: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: Some(r.get(0)?),
            track: r.get(1)?,
            survey: r.get(2)?,
            provenance: r.get(3)?,
            t_start: r.get(4)?,
            t_end: r.get(5)?,
            on_air_ns: r.get(6)?,
            f_lo: r.get(7)?,
            f_hi: r.get(8)?,
            f_center_mean: r.get(9)?,
            obw_mean: r.get(10)?,
            obw_max: r.get(11)?,
            snr_peak_max: r.get(12)?,
            snr_mean_mean: r.get(13)?,
            peak_dbfs_max: r.get(14)?,
            n: r.get(15)?,
            flags_any: r.get(16)?,
            flags_all: r.get(17)?,
            clip_count: r.get(18)?,
            dirty: false,
        })
    }

    /// Whether `c` continues this run under `p`.
    fn continues(&self, c: &Candidate, p: &DetectionRetention) -> bool {
        self.survey == c.survey
            && self.provenance == c.provenance
            && c.t_start <= self.t_end.saturating_add(p.rollup_gap_ns)
            && c.t_end >= self.t_start.saturating_sub(p.rollup_gap_ns)
            && self.t_end.max(c.t_end) - self.t_start.min(c.t_start) <= p.rollup_span_ns
    }

    /// For an untracked run, whether `c` is the same emission: it overlaps the run's frequency
    /// envelope, and joining it leaves the envelope no wider than twice the widest member. Rows no
    /// track links carry no tracker verdict that they are one signal, so frequency is the only
    /// evidence; without it two bursts at opposite edges of the window within the gap would read
    /// as one rollup spanning the window (and occupancy would learn a phantom channel at its
    /// middle).
    fn same_emission(&self, c: &Candidate) -> bool {
        let widest = self.obw_max.max(c.obw).max(c.f_hi - c.f_lo);
        c.f_lo <= self.f_hi
            && c.f_hi >= self.f_lo
            && self.f_hi.max(c.f_hi) - self.f_lo.min(c.f_lo) <= 2.0 * widest
    }

    fn add(&mut self, c: &Candidate) {
        let n = self.n as f64;
        let mean = |m: f64, x: f64| (m * n + x) / (n + 1.0);
        // T-913: the **union** of the members' intervals, not their sum. Co-timed rows — the two
        // lobes of an FSK signal in one frame, both linked to one track — otherwise count the
        // same air twice and can make `on_air_ns` exceed the hull. Candidates arrive in `t_end`
        // order, so everything already folded in is covered up to `self.t_end`: only the part of
        // `c` past that watermark is new. Exact for rows in time order; for an out-of-order row
        // it under-counts rather than double-counts, and the result is clamped to the hull.
        let covered_to = self.t_end;
        self.on_air_ns = self
            .on_air_ns
            .saturating_add((c.t_end - c.t_start.max(covered_to)).max(0));
        self.t_start = self.t_start.min(c.t_start);
        self.t_end = self.t_end.max(c.t_end);
        self.on_air_ns = self.on_air_ns.min(self.t_end - self.t_start);
        self.f_lo = self.f_lo.min(c.f_lo);
        self.f_hi = self.f_hi.max(c.f_hi);
        self.f_center_mean = mean(self.f_center_mean, c.f_center);
        self.obw_mean = mean(self.obw_mean, c.obw);
        self.obw_max = self.obw_max.max(c.obw);
        self.snr_peak_max = self.snr_peak_max.max(c.snr_peak);
        self.snr_mean_mean = mean(self.snr_mean_mean, c.snr_mean);
        self.peak_dbfs_max = self.peak_dbfs_max.max(c.peak_dbfs);
        self.n += 1;
        self.flags_any |= c.flags;
        self.flags_all &= c.flags;
        self.clip_count += c.clip_count;
        self.dirty = true;
    }

    /// [`Self::flush`], counted as a new or an extended rollup.
    fn flush_counted(
        &mut self,
        conn: &Connection,
        report: &mut PruneReport,
    ) -> Result<(), RepoError> {
        let extends = self.dirty && self.id.is_some();
        if self.flush(conn)? {
            report.rollups_inserted += 1;
        } else if extends {
            report.rollups_extended += 1;
        }
        Ok(())
    }

    /// Writes it back; returns whether it was a new row.
    fn flush(&mut self, conn: &Connection) -> Result<bool, RepoError> {
        if !self.dirty {
            return Ok(false);
        }
        self.dirty = false;
        let values = params![
            self.track,
            self.survey,
            self.provenance,
            self.t_start,
            self.t_end,
            self.on_air_ns,
            self.f_lo,
            self.f_hi,
            self.f_center_mean,
            self.obw_mean,
            self.obw_max,
            self.snr_peak_max,
            self.snr_mean_mean,
            self.peak_dbfs_max,
            self.n,
            self.flags_any,
            self.flags_all,
            self.clip_count,
        ];
        bump_extent(
            conn,
            "detection_rollup",
            self.f_hi - self.f_lo,
            self.t_end - self.t_start,
        )?;
        match self.id {
            Some(id) => {
                conn.prepare_cached(
                    "UPDATE detection_rollup SET track_id = ?1, survey_id = ?2, \
                     provenance_id = ?3, t_start = ?4, t_end = ?5, on_air_ns = ?6, f_lo = ?7, \
                     f_hi = ?8, f_center_mean = ?9, obw_mean = ?10, obw_max = ?11, \
                     snr_peak_max = ?12, snr_mean_mean = ?13, peak_dbfs_max = ?14, \
                     detections = ?15, flags_any = ?16, flags_all = ?17, clip_count = ?18 \
                     WHERE rollup_id = ?19",
                )?
                .execute(
                    &*values
                        .iter()
                        .copied()
                        .chain([&id as &dyn rusqlite::ToSql])
                        .collect::<Vec<_>>(),
                )?;
                Ok(false)
            }
            None => {
                conn.prepare_cached(
                    "INSERT INTO detection_rollup (track_id, survey_id, provenance_id, t_start, \
                     t_end, on_air_ns, f_lo, f_hi, f_center_mean, obw_mean, obw_max, \
                     snr_peak_max, snr_mean_mean, peak_dbfs_max, detections, flags_any, \
                     flags_all, clip_count) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, \
                     ?16, ?17, ?18)",
                )?
                .execute(values)?;
                self.id = Some(conn.last_insert_rowid());
                Ok(true)
            }
        }
    }
}

/// A track's cached cut: the emitters linking it when it was derived, and the lowest of their
/// cuts (`None`: unlinked).
type TrackCut = (Vec<[u8; 16]>, Option<i64>);

/// Per-pass cache of the class-1 tail cuts.
#[derive(Default)]
struct TailCuts {
    /// Emitter → its K-th newest `t_start` (`i64::MIN`: fewer than K rows, all protected).
    emitter: HashMap<[u8; 16], i64>,
    /// Track → its [`TrackCut`].
    track: HashMap<[u8; 16], TrackCut>,
}

impl TailCuts {
    fn track_cut(
        &mut self,
        conn: &Connection,
        track: [u8; 16],
        keep: usize,
    ) -> Result<Option<i64>, RepoError> {
        if let Some((_, c)) = self.track.get(&track) {
            return Ok(*c);
        }
        let emitters = emitters_of(conn, track)?;
        let mut cut: Option<i64> = None;
        for &e in &emitters {
            let k = match self.emitter.get(&e) {
                Some(&k) => k,
                None => {
                    let k = if keep == 0 {
                        i64::MAX
                    } else {
                        conn.prepare_cached(EMITTER_TAIL_CUT_SQL)?
                            .query_row(params![e, (keep - 1) as i64], |r| r.get::<_, i64>(0))
                            .optional()?
                            .unwrap_or(i64::MIN)
                    };
                    self.emitter.insert(e, k);
                    k
                }
            };
            cut = Some(cut.map_or(k, |c: i64| c.min(k)));
        }
        self.track.insert(track, (emitters, cut));
        Ok(cut)
    }

    /// Under the write lock: whether every track of `c` is still linked by exactly the emitters
    /// its cut was derived from. A track that moved is forgotten, so the next batch re-derives it.
    fn still_current(&mut self, conn: &Connection, c: &Candidate) -> Result<bool, RepoError> {
        let mut current = true;
        for t in &c.tracks {
            let now = emitters_of(conn, *t)?;
            if self.track.get(t).is_none_or(|(was, _)| *was != now) {
                self.track.remove(t);
                current = false;
            }
        }
        Ok(current)
    }

    /// Whether `c` is among the protected newest rows of an emitter linking any of its tracks.
    fn protects(
        &mut self,
        conn: &Connection,
        c: &Candidate,
        keep: usize,
    ) -> Result<bool, RepoError> {
        for &t in &c.tracks {
            if self
                .track_cut(conn, t, keep)?
                .is_some_and(|cut| c.t_start >= cut)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// The emitters currently linking `track`, sorted.
fn emitters_of(conn: &Connection, track: [u8; 16]) -> Result<Vec<[u8; 16]>, RepoError> {
    let mut v: Vec<[u8; 16]> = conn
        .prepare_cached(EMITTERS_OF_TRACK_SQL)?
        .query_map([track], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    v.sort_unstable();
    Ok(v)
}

fn tracks_of(conn: &Connection, id: [u8; 16]) -> Result<Vec<[u8; 16]>, RepoError> {
    Ok(conn
        .prepare_cached(TRACKS_OF_SQL)?
        .query_map([id], |r| r.get(0))?
        .collect::<Result<_, _>>()?)
}

fn read_candidates(
    conn: &Connection,
    survey: [u8; 16],
    cutoff: i64,
    cursor: (i64, [u8; 16]),
    batch: usize,
) -> Result<Vec<Candidate>, RepoError> {
    let mut rows: Vec<Candidate> = conn
        .prepare_cached(CANDIDATES_SQL)?
        .query_map(
            params![cutoff, cursor.0, cursor.1, batch.max(1) as i64, survey],
            |r| {
                Ok(Candidate {
                    id: r.get(0)?,
                    survey: r.get(1)?,
                    provenance: r.get(2)?,
                    t_start: r.get(3)?,
                    t_end: r.get(4)?,
                    f_center: r.get(5)?,
                    obw: r.get(6)?,
                    f_lo: r.get(7)?,
                    f_hi: r.get(8)?,
                    snr_peak: r.get(9)?,
                    snr_mean: r.get(10)?,
                    flags: r.get(11)?,
                    peak_dbfs: r.get(12)?,
                    clip_count: r.get(13)?,
                    tracks: Vec::new(),
                })
            },
        )?
        .collect::<Result<_, _>>()?;
    for c in &mut rows {
        c.tracks = tracks_of(conn, c.id)?;
    }
    Ok(rows)
}

impl Repository {
    /// Each survey's watermark: the newest `t_end` the age is measured from for its rows. An
    /// **open** survey uses its own newest row, so a tracker still running on it only ever loses
    /// rows older than its own newest by the age — a replay stamped last year into a database
    /// holding today's rows, or a host clock behind the store, cannot age out rows the tracker
    /// has yet to link. A closed or aborted survey has no tracker, so it ages from the store's
    /// newest row.
    fn survey_watermarks(&self) -> Result<Vec<([u8; 16], i64)>, RepoError> {
        let surveys: Vec<([u8; 16], bool)> = self
            .conn
            .prepare_cached("SELECT survey_id, state = 'open' FROM survey")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let mut own: Vec<([u8; 16], bool, i64)> = Vec::with_capacity(surveys.len());
        for (id, open) in surveys {
            let newest: Option<i64> = self
                .conn
                .prepare_cached("SELECT max(t_end) FROM detection WHERE survey_id = ?1")?
                .query_row([id], |r| r.get(0))?;
            if let Some(n) = newest {
                own.push((id, open, n));
            }
        }
        let store = own.iter().map(|w| w.2).max().unwrap_or(i64::MIN);
        Ok(own
            .into_iter()
            .map(|(id, open, n)| (id, if open { n } else { store }))
            .collect())
    }

    /// One retention pass over per-frame detection rows (see the module docs for what is kept and
    /// why). Batched: each batch is one short write transaction, and `between` runs before every
    /// batch after the first — return `false` from it to stop the pass early (the report then
    /// has `complete: false`; the next pass resumes from the oldest eligible row).
    pub fn prune_detections(
        &mut self,
        policy: &DetectionRetention,
        mut between: impl FnMut() -> bool,
    ) -> Result<PruneReport, RepoError> {
        let mut report = PruneReport::default();
        let age = policy.max_age_ns.max(0);
        let watermarks = self.survey_watermarks()?;
        if let Some(newest) = watermarks.iter().map(|w| w.1).max() {
            report.watermark = Some(Timestamp::from_unix_nanos(newest));
            report.cutoff = Some(Timestamp::from_unix_nanos(newest.saturating_sub(age)));
        }
        let mut cuts = TailCuts::default();
        let mut first = true;
        for (survey, watermark) in watermarks {
            let cutoff = watermark.saturating_sub(age);
            let mut cursor = (i64::MIN, [0u8; 16]);
            // T-913: rows this pass kept, so no rollup hull spans one of them.
            let mut barriers = Barriers::default();
            loop {
                if !first && !between() {
                    return Ok(report);
                }
                first = false;
                // Everything expensive happens here, before the write lock.
                let batch = read_candidates(&self.conn, survey, cutoff, cursor, policy.batch)?;
                let Some(last) = batch.last() else {
                    break;
                };
                let next_cursor = (last.t_end, last.id);
                let mut tail = Vec::with_capacity(batch.len());
                for c in &batch {
                    tail.push(cuts.protects(&self.conn, c, policy.keep_per_emitter)?);
                }
                let asked = Instant::now();
                let tx = self.write_tx()?;
                let started = Instant::now();
                report.wait_ns_max = report
                    .wait_ns_max
                    .max(started.duration_since(asked).as_nanos() as u64);
                report.examined += batch.len() as u64;
                let mut rollups = Rollups::default();
                for (c, protected) in batch.iter().zip(tail) {
                    if protected {
                        report.kept_tail += 1;
                        barriers.note(c);
                        continue;
                    }
                    if tracks_of(&tx, c.id)? != c.tracks {
                        report.kept_moved += 1;
                        barriers.note(c);
                        continue;
                    }
                    let tracked = cuts.track.len();
                    if !cuts.still_current(&tx, c)? {
                        report.relinks += (tracked - cuts.track.len()) as u64;
                        report.kept_moved += 1;
                        barriers.note(c);
                        continue;
                    }
                    let pinned: bool = tx
                        .prepare_cached(PINNED_SQL)?
                        .query_row([c.id], |r| r.get(0))?;
                    if pinned {
                        report.kept_pinned += 1;
                        barriers.note(c);
                        continue;
                    }
                    if policy.rollup {
                        rollups.add(&tx, c, policy, &barriers, &mut report)?;
                    }
                    tx.prepare_cached("DELETE FROM track_detection WHERE detection_id = ?1")?
                        .execute([c.id])?;
                    tx.prepare_cached("DELETE FROM detection WHERE detection_id = ?1")?
                        .execute([c.id])?;
                    report.deleted += 1;
                }
                rollups.flush(&tx, &mut report)?;
                tx.commit()?;
                let held = started.elapsed().as_nanos() as u64;
                report.lock_ns_max = report.lock_ns_max.max(held);
                report.lock_ns_total += held;
                report.batches += 1;
                // A pass deletes in bulk, and every deleted row touches pages of the table and
                // each of its indexes, so the WAL grows fast. Checkpoint after each batch, with
                // the lock released (PASSIVE: never waits on a reader or the detector's writer),
                // so the WAL is backfilled while the pass runs instead of ballooning to the
                // pass's whole volume.
                self.wal_checkpoint_passive()?;
                cursor = next_cursor;
            }
        }
        report.complete = true;
        Ok(report)
    }

    /// `PRAGMA wal_checkpoint(PASSIVE)`: backfills what it can without waiting on anyone. A
    /// no-op on an in-memory database.
    pub fn wal_checkpoint_passive(&self) -> Result<(), RepoError> {
        if self.conn.path().is_some_and(|p| !p.is_empty()) {
            self.conn
                .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))?;
        }
        Ok(())
    }

    /// Turns off this connection's automatic checkpoint on commit (`wal_autocheckpoint = 0`), for
    /// a connection that checkpoints itself outside its write locks — the retention pass does,
    /// so the checkpoint's I/O is never counted in, or added to, its lock hold.
    pub fn disable_wal_autocheckpoint(&self) -> Result<(), RepoError> {
        self.conn.pragma_update(None, "wal_autocheckpoint", 0)?;
        Ok(())
    }

    /// Sizes and extents of the detection store (`/api/status` `storage`), **including** the two
    /// row counts.
    pub fn detection_storage(&self) -> Result<DetectionStorage, RepoError> {
        self.detection_storage_rows(true)
    }

    /// T-913: [`Self::detection_storage`], counting the rows only if asked.
    ///
    /// `count(*)` over `detection` is a full scan of its smallest index — seconds on a store that
    /// ran unpruned for days — and it holds a read snapshot while it runs, which is exactly what
    /// keeps a WAL checkpoint from completing. The sizes, the oldest row and the watermark are all
    /// O(1) or one index seek, so the run's status refresh takes them often and the counts rarely
    /// (`hk-pipeline::retention`), saying when it last counted.
    pub fn detection_storage_rows(&self, count_rows: bool) -> Result<DetectionStorage, RepoError> {
        let pragma = |name: &str| -> Result<u64, RepoError> {
            Ok(self
                .conn
                .pragma_query_value(None, name, |r| r.get::<_, i64>(0))?
                .max(0) as u64)
        };
        let page = pragma("page_size")?;
        let wal_bytes = self
            .conn
            .path()
            .filter(|p| !p.is_empty())
            .and_then(|p| std::fs::metadata(format!("{p}-wal")).ok())
            .map(|m| m.len());
        let count = |sql: &str| -> Result<u64, RepoError> {
            Ok(self.conn.query_row(sql, [], |r| r.get::<_, i64>(0))?.max(0) as u64)
        };
        let oldest = self
            .conn
            .query_row(
                "SELECT t_start, t_end FROM detection ORDER BY t_end, detection_id LIMIT 1",
                [],
                |r| {
                    Ok(TimeRange::new(
                        Timestamp::from_unix_nanos(r.get(0)?),
                        Timestamp::from_unix_nanos(r.get(1)?),
                    ))
                },
            )
            .optional()?;
        let newest: Option<i64> =
            self.conn
                .query_row("SELECT max(t_end) FROM detection", [], |r| r.get(0))?;
        Ok(DetectionStorage {
            db_bytes: pragma("page_count")? * page,
            free_bytes: pragma("freelist_count")? * page,
            wal_bytes,
            detection_rows: if count_rows {
                count("SELECT count(*) FROM detection")?
            } else {
                0
            },
            rollup_rows: if count_rows {
                count("SELECT count(*) FROM detection_rollup")?
            } else {
                0
            },
            rows_counted: count_rows,
            oldest_detection: oldest,
            newest_detection_end: newest.map(Timestamp::from_unix_nanos),
        })
    }

    /// Rollups whose time × frequency box overlaps `region` (closed intervals), ordered by start
    /// time: what the region reads see past the retention age (module docs, class 3).
    pub fn detection_rollups_in_region(
        &self,
        region: &Region,
    ) -> Result<Vec<DetectionRollup>, RepoError> {
        let tx = self.read_tx()?;
        let b = region_bounds(&tx, "detection_rollup", region)?;
        let sql = format!(
            "SELECT {ROLLUP_COLUMNS} FROM detection_rollup \
             WHERE t_start BETWEEN ?1 AND ?2 AND t_end >= ?3 AND f_lo <= ?4 AND f_hi >= ?5 \
             ORDER BY t_start, rollup_id"
        );
        let rows = tx
            .prepare_cached(&sql)?
            .query_map(
                params![b.t_start_min, b.t1, b.t0, b.hi, b.lo],
                rollup_from_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// One track's rollups, oldest first.
    pub fn track_rollups(&self, track_id: TrackId) -> Result<Vec<DetectionRollup>, RepoError> {
        let sql = format!(
            "SELECT {ROLLUP_COLUMNS} FROM detection_rollup WHERE track_id = ?1 \
             ORDER BY t_start, rollup_id"
        );
        Ok(self
            .conn
            .prepare_cached(&sql)?
            .query_map([blob(track_id)], rollup_from_row)?
            .collect::<Result<Vec<_>, _>>()?)
    }
}
