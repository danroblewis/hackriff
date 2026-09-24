//! **Acquisition for a region analysis: the ring read, the burst set, and pin-on-analyze**
//! (MAUTO M-6, T-857; ADR-0015 §5.3, §6, §14).
//!
//! An analysis job (`/api/analyze`, M-8) searches IQ it has **already acquired**, never the live
//! stream: it is a Rule I reader (ADR-0015 §14.1 — a bounded region, processed exactly once). This
//! module is that acquisition, and nothing after it: no search, no scoring, no job.
//!
//! 1. **The burst set** ([`BurstSet::collect`], ADR-0015 §6). Given the target and the burst
//!    sightings in the window, it chooses which bursts belong to the target, guards each one
//!    (`[t_start − guard, t_end + guard]`, `guard = max(2 ms, the burst's duration)`), keeps the
//!    reads disjoint, caps them at 64 bursts or 2 s of IQ, and marks the **odd** bursts hold-out.
//! 2. **The read** ([`acquire`]). Each burst's window is read from the ring into memory through
//!    [`RingRead`] (the ADR-0015 §5.3 `IqBufferService::read`), and every chunk is entered in a
//!    [`ReadLedger`] under [`ReaderPolicy::BoundedRegion`]. The chunk's `DISCONTINUITY` is what
//!    the ledger answers (a `GAP` between bursts or where the ring had a hole) plus a segment
//!    boundary's own flags (`RETUNE`, `RATE_CHANGE`, `GAIN_CHANGE`, `PROVENANCE_CHANGE`): bursts
//!    are **concatenated with a `DISCONTINUITY` between them**, never spliced.
//! 3. **Pin on analyze** ([`acquire_and_pin`], ADR-0015 §6). The chunks just read are stored as a
//!    pinned `iq-snippet` recording with trigger `analyze` **before the search starts**, written
//!    from memory, so ring eviction cannot race the job and the clip holds exactly the samples
//!    the search reads. Its id is in [`AcquiredWindow::clip_id`], and a re-run can target it.
//!
//! # Coverage is what was read, never what was asked for
//!
//! ADR-0015 §14.5: *"`AnalyzeJob.window` already carries `segments`/`samples`/`gaps`; when the
//! engine lands it fills them **from the ledger** rather than from the requested window."*
//! [`AcquiredWindow`] is filled that way. A burst the ring had already evicted is listed in
//! [`Acquisition::missing`] with its reason and contributes no samples, no coverage and no
//! evidence; if every burst is missing the acquisition fails ([`AcquireError::NoIq`] /
//! [`AcquireError::Evicted`], the §5.1 `422 no_iq` / `410 evicted`).
//!
//! # Blind
//!
//! Membership is decided from measured identities (the emitter the inventory linked a burst to,
//! the C18 cluster) and measured boxes. Nothing here reads a band plan or a catalogue.

use std::fmt;

use hk_core::Discontinuity;
use hk_model::{Detection, EmitterId, FreqRange, RecordingId, TimeRange, Timestamp};
use hk_store::iqbuffer::IqBuffer;
use serde::Serialize;

use crate::iqbuffer::{
    ClipError, ClipExported, ClipFailure, ClipRange, IqBufferService, ReadChunk,
};
use crate::region::{LedgerError, ReadLedger, ReaderPolicy};

/// A window this short takes the burst path whatever the sightings are (ADR-0015 §6 "When").
pub const BURST_WINDOW_MAX_NS: i64 = 50_000_000;
/// The smallest guard either side of a burst (ADR-0015 §6: `guard = max(2 ms, duration)`).
pub const GUARD_MIN_NS: i64 = 2_000_000;
/// At most this many bursts are acquired (ADR-0015 §6).
pub const MAX_BURSTS: usize = 64;
/// At most this much IQ, summed over the bursts' guarded reads (ADR-0015 §6: 2 s).
pub const MAX_BURST_IQ_NS: i64 = 2_000_000_000;

/// Whether a job takes the burst path (ADR-0015 §6 "When"): the target's sightings are burst
/// detections (T-075), or the window is at most 50 ms, or a ring source names a past instant.
pub fn uses_burst_path(sightings_are_bursts: bool, window: TimeRange, past_instant: bool) -> bool {
    sightings_are_bursts || window.duration_ns() <= BURST_WINDOW_MAX_NS || past_instant
}

/// One burst seen in the window: its measured time–frequency box and, where the inventory or M3
/// has linked it, the identities it carries.
#[derive(Clone, Debug, PartialEq)]
pub struct BurstSighting {
    /// When the burst was on the air (docs/07 §4, closed).
    pub time: TimeRange,
    /// Its measured frequency extent.
    pub freq: FreqRange,
    /// The emitter the inventory linked it to, if any.
    pub emitter: Option<EmitterId>,
    /// Its C18 signature cluster (`cluster:<uuid>`), when M3 provides one.
    pub cluster_id: Option<String>,
}

impl BurstSighting {
    /// A sighting from a stored [`Detection`]: its time extent and its measured occupied
    /// bandwidth about its centre.
    pub fn from_detection(
        d: &Detection,
        emitter: Option<EmitterId>,
        cluster_id: Option<String>,
    ) -> Self {
        Self {
            time: d.time,
            freq: FreqRange::centered(d.f_center_hz, d.obw_hz),
            emitter,
            cluster_id,
        }
    }
}

/// What the job is acquiring for.
#[derive(Clone, Debug, PartialEq)]
pub struct BurstTarget {
    /// The target emitter, for an `emitter_id` target.
    pub emitter: Option<EmitterId>,
    /// The target's C18 cluster, when known.
    pub cluster_id: Option<String>,
    /// The band analysed (the emitter's measured channel, a selection's, or an ad hoc band).
    pub band: FreqRange,
}

/// Why a burst was taken as the target's (ADR-0015 §6's three ways, strongest first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Membership {
    /// Linked to the same emitter.
    Emitter,
    /// In the same C18 cluster.
    Cluster,
    /// Its box overlaps the band.
    Band,
}

impl BurstTarget {
    /// Whether `s` belongs to the target, and why.
    ///
    /// The strongest identity both sides carry decides, and a **different** identity excludes: a
    /// burst the inventory linked to another emitter is not the target's merely because it shares
    /// the band. Only when neither an emitter nor a cluster is known on both sides does the box
    /// decide.
    pub fn member(&self, s: &BurstSighting) -> Option<Membership> {
        if let (Some(a), Some(b)) = (self.emitter, s.emitter) {
            return (a == b).then_some(Membership::Emitter);
        }
        if let (Some(a), Some(b)) = (&self.cluster_id, &s.cluster_id) {
            return (a == b).then_some(Membership::Cluster);
        }
        self.band.overlaps(&s.freq).then_some(Membership::Band)
    }
}

/// One burst of the set.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Burst {
    /// Position in the set, oldest first.
    pub index: usize,
    /// The burst's own time extent (the union, when sightings overlapped).
    pub time: TimeRange,
    /// The guarded window to read. Disjoint from every other burst's.
    pub read: TimeRange,
    /// Why it is the target's.
    pub membership: Membership,
    /// Evidence from it is hold-out (ADR-0015 §6: the odd bursts).
    pub holdout: bool,
    /// The read had to be shortened below the full guard to fit [`MAX_BURST_IQ_NS`].
    pub trimmed: bool,
}

/// The bursts a job acquires (ADR-0015 §6 "Burst set").
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct BurstSet {
    /// Oldest first; reads disjoint and ascending.
    pub bursts: Vec<Burst>,
    /// Member bursts left out by the 64-burst / 2 s cap (the oldest go first).
    pub capped: usize,
    /// Member sightings that overlapped another in time and were joined into one burst.
    pub joined: usize,
}

fn guarded(time: TimeRange) -> TimeRange {
    let g = GUARD_MIN_NS.max(time.duration_ns());
    TimeRange::new(
        Timestamp::from_unix_nanos(time.start.as_unix_nanos().saturating_sub(g)),
        Timestamp::from_unix_nanos(time.end.as_unix_nanos().saturating_add(g)),
    )
}

impl BurstSet {
    /// Collects the target's bursts among `sightings` that overlap `window`.
    ///
    /// Sightings overlapping in time are one burst (two real emissions do not share a region;
    /// CLAUDE.md's overlap invariant). The **newest** bursts are kept first — they are the ones
    /// still in the ring, and the window defaults to the newest appearance (ADR-0015 §5.3) —
    /// until 64 bursts or 2 s of guarded IQ. Where two kept bursts' guards overlap, the boundary
    /// between their reads is the midpoint of the gap between the bursts, so each read still
    /// contains its whole burst, no sample is read twice (ADR-0015 §14's exactly-once) and each
    /// burst keeps its own hold-out assignment.
    pub fn collect(target: &BurstTarget, sightings: &[BurstSighting], window: TimeRange) -> Self {
        let mut members: Vec<(TimeRange, Membership)> = sightings
            .iter()
            .filter(|s| s.time.end >= s.time.start && s.time.overlaps(&window))
            .filter_map(|s| target.member(s).map(|m| (s.time, m)))
            .collect();
        members.sort_by_key(|(t, _)| (t.start, t.end));
        let mut joined = 0usize;
        let mut bursts: Vec<(TimeRange, Membership)> = Vec::with_capacity(members.len());
        for (t, m) in members {
            match bursts.last_mut() {
                Some((last, lm)) if t.start <= last.end => {
                    last.end = last.end.max(t.end);
                    // Keep the strongest reason any joined sighting gave.
                    *lm = strongest(*lm, m);
                    joined += 1;
                }
                _ => bursts.push((t, m)),
            }
        }
        // The cap, newest first, on the full guarded lengths (the midpoint split can only shorten
        // them, so the budget is never exceeded).
        let mut kept: Vec<(TimeRange, Membership, TimeRange, bool)> = Vec::new();
        let mut used = 0i64;
        for &(t, m) in bursts.iter().rev() {
            if kept.len() == MAX_BURSTS {
                break;
            }
            let read = guarded(t);
            let left = MAX_BURST_IQ_NS - used;
            if read.duration_ns() <= left {
                used += read.duration_ns();
                kept.push((t, m, read, false));
            } else if kept.is_empty() {
                // One burst longer than the budget with its guards: shrink the guards evenly, and
                // past that read the burst's first 2 s. Flagged, never silent.
                let spare = (left - t.duration_ns()).max(0) / 2;
                let start = t.start.as_unix_nanos().saturating_sub(spare);
                let read = TimeRange::new(
                    Timestamp::from_unix_nanos(start),
                    Timestamp::from_unix_nanos(start + left),
                );
                kept.push((t, m, read, true));
                break;
            } else {
                break;
            }
        }
        let capped = bursts.len() - kept.len();
        kept.reverse();
        for i in 1..kept.len() {
            let (prev_t, next_t) = (kept[i - 1].0, kept[i].0);
            if kept[i - 1].2.end > kept[i].2.start {
                let mid = prev_t.end.as_unix_nanos()
                    + (next_t.start.as_unix_nanos() - prev_t.end.as_unix_nanos()) / 2;
                let mid = Timestamp::from_unix_nanos(mid);
                kept[i - 1].2.end = mid;
                kept[i].2.start = mid;
            }
        }
        Self {
            bursts: kept
                .into_iter()
                .enumerate()
                .map(|(index, (time, membership, read, trimmed))| Burst {
                    index,
                    time,
                    read,
                    membership,
                    holdout: index % 2 == 1,
                    trimmed,
                })
                .collect(),
            capped,
            joined,
        }
    }

    /// Total guarded IQ the set asks for, ns.
    pub fn read_ns(&self) -> i64 {
        self.bursts.iter().map(|b| b.read.duration_ns()).sum()
    }
}

fn strongest(a: Membership, b: Membership) -> Membership {
    let rank = |m| match m {
        Membership::Emitter => 0,
        Membership::Cluster => 1,
        Membership::Band => 2,
    };
    if rank(b) < rank(a) { b } else { a }
}

/// The ring read (ADR-0015 §5.3): IQ of a time range, as segment chunks with provenance.
pub trait RingRead {
    /// The buffered samples of `range` (and `band`), in memory.
    fn read_ring(
        &self,
        range: TimeRange,
        band: Option<(f64, f64)>,
    ) -> Result<Vec<ReadChunk>, ClipError>;
}

/// The half-open sample-clock range a closed [`TimeRange`] reads (`end` is the instant after the
/// last sample, as a ring piece's `t1_ns` is).
fn clip_range(range: TimeRange) -> ClipRange {
    ClipRange::Time {
        t0_ns: range.start.as_unix_nanos(),
        t1_ns: range.end.as_unix_nanos(),
    }
}

impl RingRead for IqBufferService {
    fn read_ring(
        &self,
        range: TimeRange,
        band: Option<(f64, f64)>,
    ) -> Result<Vec<ReadChunk>, ClipError> {
        self.read(clip_range(range), band)
    }
}

impl RingRead for IqBuffer {
    fn read_ring(
        &self,
        range: TimeRange,
        band: Option<(f64, f64)>,
    ) -> Result<Vec<ReadChunk>, ClipError> {
        if range.end <= range.start || range.start.as_unix_nanos() < 0 {
            return Err(ClipError::Empty);
        }
        self.read(
            clip_range(range),
            band,
            hk_store::iqbuffer::DEFAULT_MAX_CLIP_BYTES,
        )
    }
}

/// One chunk of acquired IQ, in read order.
#[derive(Clone, Debug)]
pub struct AcquiredChunk {
    /// The burst it belongs to ([`Burst::index`]).
    pub burst: usize,
    /// Evidence from it is hold-out.
    pub holdout: bool,
    /// What the consumer must be told before this chunk: the ledger's answer (`STREAM_START`, a
    /// `GAP`) plus a segment boundary's own flags.
    pub discontinuity: Discontinuity,
    /// The samples and their provenance.
    pub chunk: ReadChunk,
}

/// Why a burst contributed no IQ.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Missing {
    /// Nothing is buffered there (never captured, a gap, or already evicted).
    NotBuffered,
    /// The ring overwrote it while it was being read.
    Evicted,
    /// It spans a buffer restart or a sample-rate change, which are never spliced.
    Conflict,
}

/// What the acquisition got.
#[derive(Clone, Debug)]
pub struct Acquisition {
    /// The set that was asked for.
    pub set: BurstSet,
    /// The IQ read, in order.
    pub chunks: Vec<AcquiredChunk>,
    /// The Rule I ledger: **the coverage bar is exactly its spans**.
    pub ledger: ReadLedger,
    /// Bursts that contributed nothing, with why.
    pub missing: Vec<(usize, Missing)>,
    /// The pinned clip, when pinned ([`acquire_and_pin`]).
    pub pinned: Option<ClipExported>,
}

impl Acquisition {
    /// Samples acquired.
    pub fn samples(&self) -> u64 {
        self.chunks.iter().map(|c| c.chunk.piece.samples).sum()
    }

    /// The chunks of hold-out (`true`) or search (`false`) bursts.
    pub fn split(&self, holdout: bool) -> impl Iterator<Item = &AcquiredChunk> {
        self.chunks.iter().filter(move |c| c.holdout == holdout)
    }

    /// The `window` of ADR-0015 §5.2's `AnalyzeJob`, filled from the ledger (§14.5).
    pub fn window(&self) -> AcquiredWindow {
        let spans = self.ledger.spans();
        let mut segments: Vec<u64> = self.chunks.iter().map(|c| c.chunk.piece.segment).collect();
        segments.dedup();
        AcquiredWindow {
            source: "ring",
            t_lo: spans.first().map(|s| s.start),
            t_hi: spans.last().map(|s| s.end),
            segments: segments.len(),
            samples: self.samples(),
            gaps: spans.len().saturating_sub(1),
            skipped_ns: self.ledger.skipped_ns(),
            bursts: self.set.bursts.len() - self.missing.len(),
            bursts_missing: self.missing.len(),
            bursts_capped: self.set.capped,
            clip_id: self.pinned.as_ref().map(|c| c.id),
        }
    }
}

/// ADR-0015 §5.2's `AnalyzeJob.window`, measured: what the reader **got**.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AcquiredWindow {
    /// `ring` (a live acquisition is M-8's).
    pub source: &'static str,
    /// First instant read.
    pub t_lo: Option<Timestamp>,
    /// Last instant read.
    pub t_hi: Option<Timestamp>,
    /// Distinct buffer segments read (distinct runs of provenance).
    pub segments: usize,
    /// Samples read.
    pub samples: u64,
    /// Holes between read spans (between bursts, or where the ring had none).
    pub gaps: usize,
    /// Time inside `[t_lo, t_hi]` that was not read, ns.
    pub skipped_ns: i64,
    /// Bursts that contributed IQ.
    pub bursts: usize,
    /// Bursts in the set that contributed none ([`Acquisition::missing`]).
    pub bursts_missing: usize,
    /// Member bursts the 64 / 2 s cap left out.
    pub bursts_capped: usize,
    /// The pinned clip (a `Recording` a re-run can target).
    pub clip_id: Option<RecordingId>,
}

/// Why an acquisition failed.
#[derive(Debug)]
pub enum AcquireError {
    /// The set is empty: no burst of the target in the window.
    NoBursts,
    /// No burst had retained IQ (§5.1 `422 no_iq`).
    NoIq,
    /// Every burst that had been buffered left the ring during the read (§5.1 `410 evicted`).
    Evicted,
    /// Reading the ring failed.
    Ring(ClipError),
    /// The ledger refused a span (a read out of order — a bug, never a measurement).
    Ledger(LedgerError),
    /// Pinning the clip failed.
    Pin(ClipFailure),
}

impl fmt::Display for AcquireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoBursts => f.write_str("no burst of the target in the window"),
            Self::NoIq => f.write_str("no retained IQ for any burst in the window"),
            Self::Evicted => f.write_str("the window left the ring before it could be acquired"),
            Self::Ring(e) => write!(f, "reading the ring: {e}"),
            Self::Ledger(e) => write!(f, "{e}"),
            Self::Pin(e) => write!(f, "pinning the analysis clip: {e}"),
        }
    }
}

impl std::error::Error for AcquireError {}

/// The flags a segment boundary itself carries, from the provenance either side of it.
fn boundary(prev: &ReadChunk, next: &ReadChunk) -> Discontinuity {
    if prev.piece.segment == next.piece.segment && prev.piece.run == next.piece.run {
        return Discontinuity::NONE;
    }
    let (a, b) = (&prev.piece.provenance, &next.piece.provenance);
    let mut d = Discontinuity::NONE;
    if a.tune.center_hz != b.tune.center_hz {
        d |= Discontinuity::RETUNE;
    }
    if a.tune.sample_rate_hz != b.tune.sample_rate_hz {
        d |= Discontinuity::RATE_CHANGE;
    }
    if a.tune.lna_db != b.tune.lna_db
        || a.tune.vga_db != b.tune.vga_db
        || a.tune.amp_on != b.tune.amp_on
    {
        d |= Discontinuity::GAIN_CHANGE;
    }
    if a != b {
        d |= Discontinuity::PROVENANCE_CHANGE;
    }
    if d == Discontinuity::NONE {
        // A new segment with identical provenance still restarts the stream's bookkeeping (a
        // restart, or a gap the ring recorded as a boundary): never spliced silently.
        d = Discontinuity::GAP;
    }
    d
}

/// Reads every burst of `set` from `ring` into memory (ADR-0015 §6), each chunk entered in a
/// Rule I [`ReadLedger`]. Nothing is pinned; see [`acquire_and_pin`].
pub fn acquire(
    ring: &dyn RingRead,
    set: &BurstSet,
    band: Option<(f64, f64)>,
) -> Result<Acquisition, AcquireError> {
    if set.bursts.is_empty() {
        return Err(AcquireError::NoBursts);
    }
    let mut ledger = ReadLedger::new(ReaderPolicy::BoundedRegion);
    let mut chunks: Vec<AcquiredChunk> = Vec::new();
    let mut missing = Vec::new();
    for b in &set.bursts {
        let read = match ring.read_ring(b.read, band) {
            Ok(r) => r,
            Err(ClipError::Empty) => {
                missing.push((b.index, Missing::NotBuffered));
                continue;
            }
            Err(ClipError::Evicted) => {
                missing.push((b.index, Missing::Evicted));
                continue;
            }
            Err(ClipError::MixedRates { .. } | ClipError::MixedRuns { .. }) => {
                missing.push((b.index, Missing::Conflict));
                continue;
            }
            Err(e) => return Err(AcquireError::Ring(e)),
        };
        for chunk in read {
            let span = TimeRange::new(
                Timestamp::from_unix_nanos(chunk.piece.t_ns),
                Timestamp::from_unix_nanos(chunk.piece.t1_ns),
            );
            let mut d = ledger.read(span).map_err(AcquireError::Ledger)?;
            if let Some(prev) = chunks.last() {
                d |= boundary(&prev.chunk, &chunk);
            }
            chunks.push(AcquiredChunk {
                burst: b.index,
                holdout: b.holdout,
                discontinuity: d,
                chunk,
            });
        }
    }
    if chunks.is_empty() {
        let all_evicted = missing.iter().all(|(_, m)| *m == Missing::Evicted);
        return Err(if all_evicted {
            AcquireError::Evicted
        } else {
            AcquireError::NoIq
        });
    }
    Ok(Acquisition {
        set: set.clone(),
        chunks,
        ledger,
        missing,
        pinned: None,
    })
}

/// [`acquire`], then **pin on analyze** (ADR-0015 §6): the chunks just read are stored as a pinned
/// `iq-snippet` recording (trigger `analyze`) before anything searches them. The clip is written
/// from memory, so it is byte-identical to what the search reads whatever the ring does next. A
/// pin that fails fails the acquisition: a job whose IQ could not be pinned has nothing a re-run
/// can target, and ADR-0015 §6 orders the pin before the search.
pub fn acquire_and_pin(
    service: &IqBufferService,
    set: &BurstSet,
    band: Option<(f64, f64)>,
    label: Option<&str>,
) -> Result<Acquisition, AcquireError> {
    let mut acq = acquire(service, set, band)?;
    let chunks: Vec<ReadChunk> = acq.chunks.iter().map(|c| c.chunk.clone()).collect();
    acq.pinned = Some(
        service
            .pin_chunks(&chunks, band, label)
            .map_err(AcquireError::Pin)?,
    );
    Ok(acq)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    fn t(start_ms: i64, end_ms: i64) -> TimeRange {
        TimeRange::new(
            Timestamp::from_unix_nanos(start_ms * MS),
            Timestamp::from_unix_nanos(end_ms * MS),
        )
    }

    fn sighting(start_ms: i64, end_ms: i64, f_hz: f64) -> BurstSighting {
        BurstSighting {
            time: t(start_ms, end_ms),
            freq: FreqRange::centered(f_hz, 50e3),
            emitter: None,
            cluster_id: None,
        }
    }

    fn band_target() -> BurstTarget {
        BurstTarget {
            emitter: None,
            cluster_id: None,
            band: FreqRange::centered(915e6, 100e3),
        }
    }

    #[test]
    fn the_burst_path_applies_to_bursts_short_windows_and_past_instants() {
        assert!(uses_burst_path(true, t(0, 10_000), false));
        assert!(uses_burst_path(false, t(0, 50), false));
        assert!(!uses_burst_path(false, t(0, 51), false));
        assert!(uses_burst_path(false, t(0, 10_000), true));
    }

    #[test]
    fn membership_is_decided_by_the_strongest_identity_both_sides_carry() {
        let (e1, e2) = (EmitterId::new(), EmitterId::new());
        let target = BurstTarget {
            emitter: Some(e1),
            cluster_id: Some("cluster:a".into()),
            band: FreqRange::centered(915e6, 100e3),
        };
        let mut s = sighting(0, 5, 915e6);
        assert_eq!(target.member(&s), Some(Membership::Band));
        s.emitter = Some(e1);
        assert_eq!(target.member(&s), Some(Membership::Emitter));
        // Another emitter in the same band is not the target's.
        s.emitter = Some(e2);
        assert_eq!(target.member(&s), None);
        s.emitter = None;
        s.cluster_id = Some("cluster:a".into());
        assert_eq!(target.member(&s), Some(Membership::Cluster));
        s.cluster_id = Some("cluster:b".into());
        assert_eq!(target.member(&s), None);
        // Off band, no identity: not a member.
        assert_eq!(target.member(&sighting(0, 5, 920e6)), None);
    }

    #[test]
    fn bursts_are_guarded_disjoint_and_the_odd_ones_are_hold_out() {
        // A 1 ms burst (guard 2 ms), a 10 ms burst (guard 10 ms) close behind it, one far away,
        // one off band and one outside the window.
        let sightings = [
            sighting(100, 101, 915e6),
            sighting(108, 118, 915.01e6),
            sighting(500, 505, 915e6),
            sighting(300, 305, 930e6),
            sighting(5_000, 5_001, 915e6),
        ];
        let set = BurstSet::collect(&band_target(), &sightings, t(0, 1_000));
        assert_eq!(set.bursts.len(), 3);
        let b = &set.bursts;
        assert_eq!(b[0].read.start, t(98, 98).start);
        // Guards overlapped (103 > 98): split at the midpoint of the 101–108 gap.
        assert_eq!(b[0].read.end, Timestamp::from_unix_nanos(104_500_000));
        assert_eq!(b[1].read.start, b[0].read.end);
        assert_eq!(b[1].read.end, t(128, 128).start);
        assert_eq!(b[2].read, t(495, 510));
        for x in b {
            assert!(x.read.start <= x.time.start && x.read.end >= x.time.end);
        }
        let holdout: Vec<bool> = b.iter().map(|x| x.holdout).collect();
        assert_eq!(holdout, vec![false, true, false]);
        assert_eq!((set.capped, set.joined), (0, 0));
    }

    #[test]
    fn overlapping_sightings_are_one_burst() {
        let sightings = [sighting(100, 110, 915e6), sighting(105, 120, 915e6)];
        let set = BurstSet::collect(&band_target(), &sightings, t(0, 1_000));
        assert_eq!(set.bursts.len(), 1);
        assert_eq!(set.bursts[0].time, t(100, 120));
        assert_eq!(set.joined, 1);
    }

    #[test]
    fn the_set_is_capped_at_64_bursts_keeping_the_newest() {
        let sightings: Vec<BurstSighting> = (0..100)
            .map(|i| sighting(i * 100, i * 100 + 1, 915e6))
            .collect();
        let set = BurstSet::collect(&band_target(), &sightings, t(0, 100_000));
        assert_eq!(set.bursts.len(), MAX_BURSTS);
        assert_eq!(set.capped, 36);
        assert_eq!(set.bursts[0].time, t(3_600, 3_601));
        assert_eq!(set.bursts[63].time, t(9_900, 9_901));
        assert_eq!(set.bursts[63].index, 63);
    }

    #[test]
    fn the_set_is_capped_at_2_s_of_iq() {
        // 100 ms bursts: each guarded read is 300 ms, so six fit in 2 s and the seventh does not.
        let sightings: Vec<BurstSighting> = (0..10)
            .map(|i| sighting(i * 1_000, i * 1_000 + 100, 915e6))
            .collect();
        let set = BurstSet::collect(&band_target(), &sightings, t(0, 20_000));
        assert_eq!(set.bursts.len(), 6);
        assert_eq!(set.capped, 4);
        assert!(set.read_ns() <= MAX_BURST_IQ_NS);
        assert_eq!(set.bursts[5].time, t(9_000, 9_100));
    }

    #[test]
    fn a_burst_longer_than_the_budget_is_trimmed_and_flagged() {
        let set = BurstSet::collect(&band_target(), &[sighting(0, 1_500, 915e6)], t(0, 5_000));
        let b = &set.bursts[0];
        assert!(b.trimmed);
        assert_eq!(b.read.duration_ns(), MAX_BURST_IQ_NS);
        assert!(b.read.start <= b.time.start && b.read.end >= b.time.end);
        let set = BurstSet::collect(&band_target(), &[sighting(0, 3_000, 915e6)], t(0, 5_000));
        assert_eq!(set.bursts[0].read, t(0, 2_000));
        assert!(set.bursts[0].trimmed);
    }
}
