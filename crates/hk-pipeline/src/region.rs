//! **The incremental region-decode contract** (T-265 = ADR-0017 TM-10; `docs/adr/0017` §6,
//! `docs/adr/0015` §14). CLAUDE.md invariant 5: *"decode operates on a captured region and extends
//! with it; live decoding **extends the region's time extent** and decodes only the newly-arrived
//! part (incremental), never re-decoding what is already done."*
//!
//! **Contract only.** MAUTO is unscheduled and `POST /api/analyze` still answers `501` for a
//! selection or band target, so nothing here runs a decoder, reads the ring or spawns a thread.
//! What it does is make the four mistakes ADR-0017 §6 names **unrepresentable**, so that the
//! engine written later cannot commit them quietly:
//!
//! | ADR-0017 §6 | The mistake | Where it is refused here |
//! |---|---|---|
//! | §6.2 Rule I | re-decoding samples already decoded | [`RegionJob::extend_to`] enqueues only `[old_end, new_end]` and answers `None` for an end it has already handed out; [`ReadLedger::read`] refuses a span that overlaps one already read |
//! | §6.3 fuse onto the **region** reader | audio acquires the batch job's backlog | [`PipelinePlan::validate`] ⇒ [`PlanError::AudioOnBoundedRegion`] |
//! | §6.3 fuse onto the **live-edge** reader | the live-edge skip silently discards the samples the incremental job promised never to re-decode | [`ReadLedger::skip_to`] ⇒ [`LedgerError::SkipForbidden`] under [`ReaderPolicy::BoundedRegion`] |
//! | §6.3 handover | an implicit merge, and a coverage bar with silent holes | [`RegionJob::hand_over`] is the only route out of [`RegionPhase::Region`]; it is a **named** state carrying a [`Discontinuity`] |
//! | §6.4 sibling outputs | RDS evidence `n` inflated by time the audio reader skipped | [`PipelinePlan::policy_for_output`] has no per-output override; [`ReadLedger::credit`] refuses a span that was not read |
//!
//! # One pipeline, one reader, one policy
//!
//! > *"A pipeline has exactly one input reader, so it is exactly one of the two. The model forbids
//! > a pipeline being both."* — ADR-0017 §6.2
//!
//! [`ReaderPolicy`] is therefore a field of [`PipelinePlan`] and of [`ReadLedger`], not of an
//! output, a recipe or a signal. The policy is not a property of *what* is being decoded — the
//! same FM station, the same recipe and the same channel can be read both ways at once — it is a
//! property of **where the reader is attached**. §6.3's "decode a growing region while listening
//! live" is two `PipelinePlan`s and two `ReadLedger`s, and there is deliberately no constructor
//! in this module that yields one of either holding both policies.
//!
//! # Coverage is exactly the samples that were read
//!
//! [`ReadLedger`] is the whole honesty mechanism. A [`RegionJob`]'s extent is what the user asked
//! for; its **coverage** is what the reader got, which is less whenever the ring had evicted part
//! of the window, a segment boundary fell inside it, or a handover abandoned enqueued work. The
//! difference is recorded as skipped time and flagged, never rounded up into the extent: *"a
//! coverage bar that silently has holes in it is worse than no coverage bar"* (§6.3).
//!
//! # Closed intervals, and what "contiguous" means
//!
//! Spans are [`hk_model::TimeRange`], which is **closed** `[start, end]` like every other time
//! extent in docs/07 §4. Two spans are *contiguous* when the later starts exactly at the earlier's
//! `end` (they meet at one instant of zero duration, so summed durations still equal the union's);
//! they **overlap** only when it starts strictly before, which is the re-read [`ReadLedger::read`]
//! refuses; and there is a **gap** when it starts strictly after. This is the same rule
//! [`TimeRange::overlaps`] uses, read one instant tighter because a work span's end is the next
//! work span's start by construction.
//!

use std::collections::VecDeque;
use std::fmt;

use hk_core::Discontinuity;
use hk_model::{Region, TimeRange, Timestamp};

/// Which of ADR-0017 §6's two rules one pipeline's single input reader obeys.
///
/// Not a property of the recipe, the emitter or the output kind — a property of **where the
/// reader is attached**. One pipeline has one reader, so it has exactly one of these.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ReaderPolicy {
    /// **Rule L — latency wins at the live edge.** The reader is attached at the live edge and
    /// obeys ADR-0011 §8.5 unchanged: it never accumulates a backlog, it **skips forward** when it
    /// falls further than `max_backlog_ns` behind the writer, and every skip is counted and
    /// flagged `DISCONTINUITY`. This governs Listen and anything else a human is consuming in real
    /// time. *"Latency is a contract for audio and merely a statistic for decoding."*
    ///
    /// The backlog bound is carried rather than defaulted: the runtime's value is
    /// [`crate::ListenConfig::max_backlog_s`] (or a recipe's `input.liveness.max_backlog_s`), and a
    /// second spelling of it here would be a new drift surface.
    LiveEdge {
        /// Largest backlog behind the writer before the reader seeks to the live edge, ns.
        max_backlog_ns: i64,
    },
    /// **Rule I — the region is the unit of work.** The reader is a **bounded region** of the IQ
    /// ring and processes `[t_start, t_end]` exactly once. Extending the region enqueues only
    /// `[old_end, new_end]`; nothing already processed is re-processed; nothing is ever skipped to
    /// stay current. This governs `POST /api/analyze`, the ADR-0015 §6 burst path, and any
    /// user-captured region.
    BoundedRegion,
}

impl ReaderPolicy {
    /// Rule L with the backlog bound given in seconds.
    pub fn live_edge_secs(max_backlog_s: f64) -> Self {
        Self::LiveEdge {
            max_backlog_ns: (max_backlog_s * 1e9) as i64,
        }
    }

    /// The reader is attached at the live edge (Rule L).
    pub fn is_live_edge(self) -> bool {
        matches!(self, Self::LiveEdge { .. })
    }

    /// The reader may seek forward past unread samples to stay current.
    ///
    /// **Rule L only.** Under Rule I a skip would discard the very samples the job promised to
    /// process exactly once, and the job would then report complete coverage over a region with
    /// holes in it — §6.3's second fusion failure, "always the worst kind".
    pub fn may_skip(self) -> bool {
        self.is_live_edge()
    }

    /// The name used on the wire and in ADR-0011 §8.5's `input.liveness.mode`.
    pub fn name(self) -> &'static str {
        match self {
            Self::LiveEdge { .. } => "live-edge",
            Self::BoundedRegion => "bounded-region",
        }
    }
}

/// What one output of a pipeline is for.
///
/// The distinction exists only because ADR-0017 §6.4 hangs a rule on it — an `audio` output has a
/// human waiting on it — never so that an output can choose its own reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputRole {
    /// An ADR-0011 §8.3 `audio` output: a human is listening in real time.
    Audio,
    /// Any decode output — `bits`, `symbols`, `messages`, `frame`. The RDS `messages` output of
    /// ADR-0011 §8.9's FM recipe is one of these, hanging off the same node as the `audio`.
    Decode,
}

/// One pipeline: **one** input reader, and the outputs hanging off it.
///
/// The type is the ADR-0017 §6.2 ruling in structural form — `policy` is a single field, so a plan
/// cannot declare both rules, and [`policy_for_output`](Self::policy_for_output) has no override
/// parameter, so an output cannot claim a policy of its own.
#[derive(Clone, Debug, PartialEq)]
pub struct PipelinePlan {
    policy: ReaderPolicy,
    outputs: Vec<OutputRole>,
}

impl PipelinePlan {
    /// A Rule L pipeline: its reader is attached at the live edge.
    pub fn live_edge(max_backlog_ns: i64) -> Self {
        Self {
            policy: ReaderPolicy::LiveEdge { max_backlog_ns },
            outputs: Vec::new(),
        }
    }

    /// A Rule I pipeline: its reader is a bounded region of the ring.
    pub fn bounded_region() -> Self {
        Self {
            policy: ReaderPolicy::BoundedRegion,
            outputs: Vec::new(),
        }
    }

    /// Declares an output. Builder form; validate afterwards.
    #[must_use]
    pub fn with_output(mut self, role: OutputRole) -> Self {
        self.outputs.push(role);
        self
    }

    /// The one reader's policy.
    pub fn policy(&self) -> ReaderPolicy {
        self.policy
    }

    /// The declared outputs, in declaration order.
    pub fn outputs(&self) -> &[OutputRole] {
        &self.outputs
    }

    /// The liveness policy in force for `role`.
    ///
    /// **ADR-0017 §6.4:** *"a decode output that is a sibling of an audio output inherits the audio
    /// reader's liveness policy."* It is the same answer for every role because the outputs share
    /// the one reader; the signature takes `role` only so that call sites read as the rule rather
    /// than as an implementation detail. A user who wants gapless RDS runs a region job over the
    /// ring — Rule I, a **second** pipeline, and exactly the §6.3 shape.
    pub fn policy_for_output(&self, role: OutputRole) -> ReaderPolicy {
        let _ = role;
        self.policy
    }

    /// Checks the two rules a plan can break on its own.
    ///
    /// The third — fusing a region job onto a live-edge reader — is not expressible as a plan at
    /// all; it is refused at the moment it would happen, by [`ReadLedger::skip_to`].
    pub fn validate(&self) -> Result<(), PlanError> {
        let audio = self
            .outputs
            .iter()
            .filter(|r| **r == OutputRole::Audio)
            .count();
        if audio > 1 {
            return Err(PlanError::MultipleAudioOutputs);
        }
        if audio > 0 && !self.policy.is_live_edge() {
            return Err(PlanError::AudioOnBoundedRegion);
        }
        Ok(())
    }
}

/// Why a [`PipelinePlan`] is not a legal pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// An `audio` output on a bounded-region reader: ADR-0017 §6.3's first fusion. *"Fuse onto the
    /// region reader and the audio acquires the batch job's backlog. Everything still decodes; it
    /// just lags."* ADR-0015 §12.10 calls that the highest-risk item in its plan, and no existing
    /// test would catch it. Listen is a live audio tap, not a decode of a captured region.
    AudioOnBoundedRegion,
    /// More than one `audio` output in one pipeline (ADR-0011 §8.4: the liveness and listener
    /// budget are per pipeline).
    MultipleAudioOutputs,
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::AudioOnBoundedRegion => {
                "an audio output needs a live-edge reader; a bounded-region reader would give it \
                 the region job's backlog (ADR-0017 §6.3)"
            }
            Self::MultipleAudioOutputs => "at most one audio output per pipeline (ADR-0011 §8.4)",
        };
        f.write_str(s)
    }
}

impl std::error::Error for PlanError {}

/// What a reader **actually read**, what it did not, and the evidence credited from it.
///
/// Both policies keep one, because both can have holes — Rule L from its own skips, Rule I from
/// ring eviction and segment boundaries — and in both cases the hole has to be visible. The two
/// invariants it enforces:
///
/// 1. **Nothing is read twice.** [`read`](Self::read) refuses a span overlapping one already read,
///    which is CLAUDE.md invariant 5 at the only layer that can check it.
/// 2. **Evidence counts only what was read.** [`credit`](Self::credit) refuses a span that is not
///    covered, so the ADR-0015 §2.2 bits ladder cannot be inflated by samples that were never read
///    (ADR-0017 §6.4).
#[derive(Clone, Debug)]
pub struct ReadLedger {
    policy: ReaderPolicy,
    read: Vec<TimeRange>,
    skipped_ns: i64,
    skips: u32,
    evidence_bits: u64,
}

impl ReadLedger {
    /// An empty ledger for a reader obeying `policy`.
    pub fn new(policy: ReaderPolicy) -> Self {
        Self {
            policy,
            read: Vec::new(),
            skipped_ns: 0,
            skips: 0,
            evidence_bits: 0,
        }
    }

    /// The policy of the reader this ledger belongs to.
    pub fn policy(&self) -> ReaderPolicy {
        self.policy
    }

    /// Records a span the reader actually read, and answers the flags the next output chunk
    /// carries.
    ///
    /// - The first span is flagged [`Discontinuity::STREAM_START`].
    /// - A span starting exactly where the last one ended is contiguous: no flag, and the two
    ///   merge into one covered span.
    /// - A span starting **after** the last one ended leaves a hole: the gap is counted as skipped
    ///   time and the span is flagged [`Discontinuity::GAP`]. Under Rule I that is a measurement
    ///   fact (the ring had evicted it, or a segment boundary fell there), never a decision.
    /// - A span starting **before** the last one ended is a re-read and is refused.
    pub fn read(&mut self, span: TimeRange) -> Result<Discontinuity, LedgerError> {
        if span.end < span.start {
            return Err(LedgerError::Inverted);
        }
        let Some(last) = self.read.last_mut() else {
            self.read.push(span);
            return Ok(Discontinuity::STREAM_START);
        };
        if span.start < last.end {
            return Err(LedgerError::AlreadyRead);
        }
        if span.start == last.end {
            last.end = span.end;
            return Ok(Discontinuity::NONE);
        }
        self.skipped_ns += span.start.as_unix_nanos() - last.end.as_unix_nanos();
        self.read.push(span);
        Ok(Discontinuity::GAP)
    }

    /// **Rule L only.** Seeks the reader forward to `t`, discarding everything between the last
    /// span read and `t`.
    ///
    /// Under [`ReaderPolicy::BoundedRegion`] this is [`LedgerError::SkipForbidden`]: it is
    /// ADR-0017 §6.3's second fusion, where *"the live-edge skip silently discards the very samples
    /// the incremental job promised never to re-decode"* and the job then reports complete coverage
    /// over a region with holes in it.
    ///
    /// The skip is counted and the next [`read`](Self::read) from `t` continues without being
    /// counted a second time.
    pub fn skip_to(&mut self, t: Timestamp) -> Result<Discontinuity, LedgerError> {
        if !self.policy.may_skip() {
            return Err(LedgerError::SkipForbidden);
        }
        let last_end = match self.read.last() {
            Some(last) => last.end,
            None => {
                // Nothing read yet: there is no backlog to discard, so attaching at `t` is not a
                // skip. Start the ledger there.
                self.read.push(TimeRange::instant(t));
                return Ok(Discontinuity::STREAM_START);
            }
        };
        if t < last_end {
            return Err(LedgerError::SkipBackwards);
        }
        if t == last_end {
            return Ok(Discontinuity::NONE);
        }
        self.skipped_ns += t.as_unix_nanos() - last_end.as_unix_nanos();
        self.skips += 1;
        self.read.push(TimeRange::instant(t));
        Ok(Discontinuity::GAP)
    }

    /// Credits `bits` of evidence recovered from `span`.
    ///
    /// Refuses a span not wholly covered by what was read ([`LedgerError::NotRead`]). That is
    /// ADR-0017 §6.4's rule — **evidence `n` does not count skipped time** — expressed as the only
    /// thing that can enforce it: you cannot claim bits from samples the reader never had.
    pub fn credit(&mut self, span: TimeRange, bits: u64) -> Result<(), LedgerError> {
        if span.end < span.start {
            return Err(LedgerError::Inverted);
        }
        let covered = self
            .read
            .iter()
            .any(|r| r.start <= span.start && r.end >= span.end);
        if !covered {
            return Err(LedgerError::NotRead);
        }
        self.evidence_bits += bits;
        Ok(())
    }

    /// Evidence credited so far, in ADR-0015 §2.2 significance bits.
    pub fn evidence_bits(&self) -> u64 {
        self.evidence_bits
    }

    /// The disjoint, ordered spans actually read. **This is the coverage bar**, and it is exactly
    /// the samples that were read — never the extent that was asked for.
    pub fn spans(&self) -> &[TimeRange] {
        &self.read
    }

    /// Total time read, ns. Contiguous spans are merged, so this is the union's duration.
    pub fn covered_ns(&self) -> i64 {
        self.read.iter().map(TimeRange::duration_ns).sum()
    }

    /// Total time inside the reader's own span that was never read, ns: Rule L's skips plus any
    /// gap between spans.
    pub fn skipped_ns(&self) -> i64 {
        self.skipped_ns
    }

    /// Rule L skips taken (each one a flagged `DISCONTINUITY`). Always 0 under Rule I.
    pub fn skips(&self) -> u32 {
        self.skips
    }
}

/// Why a [`ReadLedger`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerError {
    /// `end` is before `start`.
    Inverted,
    /// The span overlaps one already read. CLAUDE.md invariant 5: never re-decode what is done.
    AlreadyRead,
    /// A live-edge skip on a bounded-region reader (ADR-0017 §6.3).
    SkipForbidden,
    /// A skip to an instant already passed.
    SkipBackwards,
    /// Evidence claimed from a span the reader never read (ADR-0017 §6.4).
    NotRead,
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Inverted => "span ends before it starts",
            Self::AlreadyRead => "span overlaps one already read; nothing is decoded twice",
            Self::SkipForbidden => {
                "a bounded-region reader may not skip forward: it would discard samples the job \
                 promised to process exactly once (ADR-0017 §6.3)"
            }
            Self::SkipBackwards => "a skip may not move backwards",
            Self::NotRead => "evidence may not be credited from samples that were not read",
        };
        f.write_str(s)
    }
}

impl std::error::Error for LedgerError {}

/// Where a [`RegionJob`] is in its life. **There are two states and one transition between them**,
/// and the transition is [`RegionJob::hand_over`] — there is no implicit merge into the live
/// pipeline (ADR-0017 §6.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionPhase {
    /// Rule I: the region job owns the output, and its coverage advances behind the live edge as
    /// the region's time extent is extended.
    Region,
    /// The **named** handover state: the region job has ended and the live pipeline's output
    /// continues. Reached only through [`RegionJob::hand_over`], which produces the [`Handover`]
    /// record describing what, if anything, fell between the two.
    HandedOver {
        /// The last instant the region job covered.
        at: Timestamp,
        /// The first instant the live pipeline's reader covers.
        live_from: Timestamp,
    },
}

/// The record of one handover from a region job to a live pipeline.
///
/// *"If the batch catches up to the edge, the UI may hand over — the region job ends and the live
/// pipeline's output continues — but a handover is an explicit, named state, carrying a
/// `DISCONTINUITY` if any samples fell between the two, never an implicit merge."* (ADR-0017 §6.3)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Handover {
    /// The last instant the region job covered.
    pub at: Timestamp,
    /// The first instant the live pipeline's reader covers.
    pub live_from: Timestamp,
    /// The span that fell between the two readers, if any. `None` is a seamless handover.
    pub gap: Option<TimeRange>,
    /// Work that had been enqueued on the region job and was never read, ns. A handover with work
    /// outstanding leaves a hole in the coverage bar, and the hole is named rather than rounded
    /// away.
    pub abandoned_ns: i64,
    /// [`Discontinuity::GAP`] when anything — a gap or abandoned work — was lost at the handover;
    /// [`Discontinuity::NONE`] when it was seamless.
    pub flags: Discontinuity,
}

impl Handover {
    /// Nothing was lost between the two readers.
    pub fn is_seamless(&self) -> bool {
        self.flags == Discontinuity::NONE
    }
}

/// A **bounded-region** decode job: Rule I, one reader, work handed out exactly once.
///
/// The object the user made when they captured a time–frequency section to decode. Its `region`
/// is a docs/07 §4 [`Region`] — a frequency extent and a time extent — and the time extent is the
/// thing that **grows**: live decoding extends it, and extending it enqueues only the newly
/// arrived part.
///
/// ```
/// use hk_model::{FreqRange, Region, TimeRange, Timestamp};
/// use hk_pipeline::region::RegionJob;
///
/// let s = |ns| Timestamp::from_unix_nanos(ns);
/// let freq = FreqRange::new(433.9e6, 434.1e6);
/// let mut job = RegionJob::open(Region::new(freq, TimeRange::new(s(0), s(1_000)))).unwrap();
///
/// // The opening region is the first work item, handed out once.
/// let first = job.next_work().unwrap();
/// assert_eq!(first, TimeRange::new(s(0), s(1_000)));
/// job.complete(first).unwrap();
///
/// // Extending enqueues ONLY [old_end, new_end] — never the part already decoded.
/// assert_eq!(job.extend_to(s(1_500)).unwrap(), Some(TimeRange::new(s(1_000), s(1_500))));
/// // An end already handed out is not work.
/// assert_eq!(job.extend_to(s(1_200)).unwrap(), None);
/// ```
#[derive(Clone, Debug)]
pub struct RegionJob {
    region: Region,
    enqueued_to: Timestamp,
    pending: VecDeque<TimeRange>,
    ledger: ReadLedger,
    phase: RegionPhase,
}

impl RegionJob {
    /// Opens a job over `region`. Its time extent is enqueued as the first work item.
    pub fn open(region: Region) -> Result<Self, RegionError> {
        if region.time.end < region.time.start {
            return Err(RegionError::Inverted);
        }
        let mut job = Self {
            region,
            enqueued_to: region.time.start,
            pending: VecDeque::new(),
            ledger: ReadLedger::new(ReaderPolicy::BoundedRegion),
            phase: RegionPhase::Region,
        };
        job.extend_to(region.time.end)?;
        Ok(job)
    }

    /// The region as it stands: frequency extent, and the time extent as extended so far.
    pub fn region(&self) -> Region {
        self.region
    }

    /// The time extent as extended so far. What the user asked for — **not** what was read; for
    /// that, [`coverage`](Self::coverage).
    pub fn extent(&self) -> TimeRange {
        self.region.time
    }

    /// The single reader's policy. Always [`ReaderPolicy::BoundedRegion`]: there is no constructor
    /// that gives a region job a live-edge reader, because that is §6.3's second fusion.
    pub fn policy(&self) -> ReaderPolicy {
        self.ledger.policy()
    }

    /// Where the job is (ADR-0017 §6.3).
    pub fn phase(&self) -> RegionPhase {
        self.phase
    }

    /// **Rule I.** Extends the region's time extent to `new_end` and enqueues **only**
    /// `[old_end, new_end]`.
    ///
    /// - `Ok(Some(span))` — that span is new work, handed out exactly once for the lifetime of the
    ///   job.
    /// - `Ok(None)` — `new_end` is at or before the end already enqueued, so there is nothing new.
    ///   The extent never shrinks and already-decoded time is never re-enqueued.
    /// - [`RegionError::HandedOver`] — the job ended at a handover; extending it now would mean
    ///   two readers over the same samples, which is what §6.3 forbids. Open a new job.
    pub fn extend_to(&mut self, new_end: Timestamp) -> Result<Option<TimeRange>, RegionError> {
        if let RegionPhase::HandedOver { .. } = self.phase {
            return Err(RegionError::HandedOver);
        }
        if new_end <= self.enqueued_to {
            return Ok(None);
        }
        let span = TimeRange::new(self.enqueued_to, new_end);
        self.enqueued_to = new_end;
        self.region.time.end = new_end;
        self.pending.push_back(span);
        Ok(Some(span))
    }

    /// The next enqueued work item, without consuming it. Consumed by
    /// [`complete`](Self::complete) or [`abandon`](Self::abandon).
    pub fn next_work(&self) -> Option<TimeRange> {
        self.pending.front().copied()
    }

    /// Enqueued work not yet read, ns.
    pub fn pending_ns(&self) -> i64 {
        self.pending.iter().map(TimeRange::duration_ns).sum()
    }

    /// Records that the front work item was read as `read`, and answers the flags its output
    /// chunk carries.
    ///
    /// `read` may be **shorter** than the work item at either end — the ring had evicted the head,
    /// or a segment boundary cut the tail. That is the normal case, not an error: the shortfall
    /// becomes a hole in the coverage, flagged on the next span, and is never quietly counted as
    /// covered. It may not be *longer*, and it may not reach outside the item.
    pub fn complete(&mut self, read: TimeRange) -> Result<Discontinuity, RegionError> {
        if let RegionPhase::HandedOver { .. } = self.phase {
            return Err(RegionError::HandedOver);
        }
        let work = self.pending.front().copied().ok_or(RegionError::NoWork)?;
        if read.start < work.start || read.end > work.end || read.end < read.start {
            return Err(RegionError::OutsideWork);
        }
        let flags = self.ledger.read(read)?;
        self.pending.pop_front();
        Ok(flags)
    }

    /// Drops the front work item **unread**, counting it as a hole rather than as coverage.
    ///
    /// The honest answer when the ring evicted the whole item before the job reached it.
    pub fn abandon(&mut self) -> Option<TimeRange> {
        self.pending.pop_front()
    }

    /// Credits `bits` of evidence from `span`. Refuses a span the job never read.
    pub fn credit(&mut self, span: TimeRange, bits: u64) -> Result<(), RegionError> {
        self.ledger.credit(span, bits).map_err(RegionError::Ledger)
    }

    /// What the job actually read — the coverage bar, the evidence ledger and the holes.
    pub fn coverage(&self) -> &ReadLedger {
        &self.ledger
    }

    /// Ends the job and hands its output over to a live pipeline whose reader covers from
    /// `live_from`.
    ///
    /// **The only transition out of [`RegionPhase::Region`], and it is a named state.** The
    /// returned [`Handover`] says exactly what fell between the two readers: a gap when the live
    /// reader starts after the job's coverage ended, and any work still enqueued and unread. Either
    /// puts [`Discontinuity::GAP`] on the record. Nothing here merges the two readers, and nothing
    /// lets the job continue afterwards.
    pub fn hand_over(&mut self, live_from: Timestamp) -> Result<Handover, RegionError> {
        if let RegionPhase::HandedOver { .. } = self.phase {
            return Err(RegionError::HandedOver);
        }
        let at = self
            .ledger
            .spans()
            .last()
            .map(|s| s.end)
            .unwrap_or(self.region.time.start);
        let abandoned_ns = self.pending_ns();
        self.pending.clear();
        let gap = (live_from > at).then(|| TimeRange::new(at, live_from));
        let flags = if gap.is_some() || abandoned_ns > 0 {
            Discontinuity::GAP
        } else {
            Discontinuity::NONE
        };
        self.phase = RegionPhase::HandedOver { at, live_from };
        Ok(Handover {
            at,
            live_from,
            gap,
            abandoned_ns,
            flags,
        })
    }
}

/// Why a [`RegionJob`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionError {
    /// The region's time extent ends before it starts.
    Inverted,
    /// The job has handed over; it owns no samples any more (ADR-0017 §6.3).
    HandedOver,
    /// Nothing is enqueued.
    NoWork,
    /// The completed span is not inside the work item it completes.
    OutsideWork,
    /// The coverage ledger refused.
    Ledger(LedgerError),
}

impl From<LedgerError> for RegionError {
    fn from(e: LedgerError) -> Self {
        Self::Ledger(e)
    }
}

impl fmt::Display for RegionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inverted => f.write_str("region time extent ends before it starts"),
            Self::HandedOver => {
                f.write_str("the region job handed over to the live pipeline; open a new job")
            }
            Self::NoWork => f.write_str("no work is enqueued"),
            Self::OutsideWork => f.write_str("the completed span is outside its work item"),
            Self::Ledger(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RegionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::FreqRange;

    fn t(ns: i64) -> Timestamp {
        Timestamp::from_unix_nanos(ns)
    }

    fn span(lo: i64, hi: i64) -> TimeRange {
        TimeRange::new(t(lo), t(hi))
    }

    fn job(lo: i64, hi: i64) -> RegionJob {
        RegionJob::open(Region::new(FreqRange::new(433.9e6, 434.1e6), span(lo, hi))).unwrap()
    }

    /// CLAUDE.md invariant 5: extending the region enqueues only the newly-arrived part, and the
    /// union of every work item ever handed out is the extent, each sample exactly once.
    #[test]
    fn extending_enqueues_only_the_new_part() {
        let mut j = job(0, 1_000);
        let first = j.next_work().unwrap();
        assert_eq!(first, span(0, 1_000));
        j.complete(first).unwrap();

        let mut handed_out = vec![first];
        for end in [1_500, 2_000, 2_400] {
            let w = j.extend_to(t(end)).unwrap().expect("new work");
            handed_out.push(w);
            j.complete(w).unwrap();
        }
        assert_eq!(
            handed_out,
            vec![
                span(0, 1_000),
                span(1_000, 1_500),
                span(1_500, 2_000),
                span(2_000, 2_400)
            ]
        );
        // Contiguous and exactly once: the durations sum to the extent, with no overlap.
        let total: i64 = handed_out.iter().map(TimeRange::duration_ns).sum();
        assert_eq!(total, j.extent().duration_ns());
        assert_eq!(j.extent(), span(0, 2_400));
        assert_eq!(j.coverage().covered_ns(), 2_400);
        assert_eq!(j.coverage().skipped_ns(), 0);
    }

    /// An extension that does not reach past what was already enqueued is not work. Nothing
    /// already decoded is re-decoded, and the extent never shrinks.
    #[test]
    fn re_extending_never_re_decodes() {
        let mut j = job(0, 1_000);
        let w = j.next_work().unwrap();
        j.complete(w).unwrap();
        j.extend_to(t(2_000)).unwrap().expect("new work");

        assert_eq!(j.extend_to(t(2_000)).unwrap(), None);
        assert_eq!(j.extend_to(t(1_500)).unwrap(), None);
        assert_eq!(j.extend_to(t(0)).unwrap(), None);
        assert_eq!(j.extent(), span(0, 2_000));
        assert_eq!(j.pending_ns(), 1_000);
    }

    /// The ledger is the last line of defence: a span overlapping one already read is refused
    /// whoever asks.
    #[test]
    fn a_span_is_never_read_twice() {
        let mut l = ReadLedger::new(ReaderPolicy::BoundedRegion);
        l.read(span(0, 1_000)).unwrap();
        assert_eq!(l.read(span(500, 1_500)), Err(LedgerError::AlreadyRead));
        assert_eq!(l.read(span(0, 100)), Err(LedgerError::AlreadyRead));
        // Meeting at one instant is contiguous, not an overlap, and the spans merge.
        assert_eq!(l.read(span(1_000, 1_500)).unwrap(), Discontinuity::NONE);
        assert_eq!(l.spans(), [span(0, 1_500)]);
        assert_eq!(l.covered_ns(), 1_500);
    }

    /// "A region job's coverage is exactly the samples it read." Eviction shortens a work item;
    /// the shortfall is a flagged hole, never coverage.
    #[test]
    fn coverage_is_exactly_what_was_read() {
        let mut j = job(0, 1_000);
        let w = j.next_work().unwrap();
        // The ring had evicted the first 400 ns of the window.
        assert_eq!(
            j.complete(span(400, 1_000)).unwrap(),
            Discontinuity::STREAM_START
        );
        assert!(w.duration_ns() > j.coverage().covered_ns());
        assert_eq!(j.coverage().covered_ns(), 600);

        let w2 = j.extend_to(t(2_000)).unwrap().unwrap();
        // A segment boundary cut the head of the next item too: the hole is flagged.
        assert_eq!(j.complete(span(1_200, 2_000)).unwrap(), Discontinuity::GAP);
        assert_eq!(j.coverage().covered_ns(), 600 + 800);
        assert_eq!(j.coverage().skipped_ns(), 200);
        assert_eq!(w2, span(1_000, 2_000));
        assert_eq!(j.extent().duration_ns(), 2_000);
    }

    /// A completed span may not reach outside its work item.
    #[test]
    fn a_completion_stays_inside_its_work_item() {
        let mut j = job(0, 1_000);
        assert_eq!(j.complete(span(0, 1_200)), Err(RegionError::OutsideWork));
        assert_eq!(
            j.complete(span(1_200, 1_400)),
            Err(RegionError::OutsideWork)
        );
        j.complete(span(0, 1_000)).unwrap();
        assert_eq!(j.complete(span(0, 1_000)), Err(RegionError::NoWork));
    }

    /// ADR-0017 §6.3, second fusion: a bounded-region reader may not take the live-edge skip.
    #[test]
    fn a_region_reader_may_not_skip_to_the_live_edge() {
        let mut l = ReadLedger::new(ReaderPolicy::BoundedRegion);
        l.read(span(0, 1_000)).unwrap();
        assert_eq!(l.skip_to(t(9_000)), Err(LedgerError::SkipForbidden));
        assert_eq!(l.covered_ns(), 1_000);
        assert_eq!(l.skips(), 0);
        assert!(!ReaderPolicy::BoundedRegion.may_skip());
    }

    /// ADR-0017 §6.3, first fusion: an audio output on a bounded-region reader would acquire the
    /// batch job's backlog.
    #[test]
    fn audio_needs_a_live_edge_reader() {
        let region = PipelinePlan::bounded_region().with_output(OutputRole::Audio);
        assert_eq!(region.validate(), Err(PlanError::AudioOnBoundedRegion));

        let listen = PipelinePlan::live_edge(600_000_000).with_output(OutputRole::Audio);
        assert_eq!(listen.validate(), Ok(()));

        let two_audio = PipelinePlan::live_edge(600_000_000)
            .with_output(OutputRole::Audio)
            .with_output(OutputRole::Audio);
        assert_eq!(two_audio.validate(), Err(PlanError::MultipleAudioOutputs));

        // A decode-only bounded-region pipeline is the region job, and is legal.
        let decode = PipelinePlan::bounded_region().with_output(OutputRole::Decode);
        assert_eq!(decode.validate(), Ok(()));
    }

    /// ADR-0017 §6.4: RDS hanging off the FM recipe's `fm` node inherits the audio reader's
    /// policy. One reader, so one policy, with no per-output override.
    #[test]
    fn a_sibling_decode_output_inherits_the_audio_readers_policy() {
        let fm = PipelinePlan::live_edge(600_000_000)
            .with_output(OutputRole::Audio)
            .with_output(OutputRole::Decode);
        fm.validate().unwrap();
        assert_eq!(fm.policy_for_output(OutputRole::Decode), fm.policy());
        assert_eq!(
            fm.policy_for_output(OutputRole::Decode),
            fm.policy_for_output(OutputRole::Audio)
        );
        assert!(fm.policy_for_output(OutputRole::Decode).is_live_edge());
    }

    /// ADR-0017 §6.4: the audio reader skips, so RDS text has a gap — and evidence `n` does not
    /// count the skipped time.
    #[test]
    fn evidence_n_does_not_count_skipped_time() {
        let mut l = ReadLedger::new(ReaderPolicy::live_edge_secs(0.6));
        l.read(span(0, 1_000)).unwrap();
        l.credit(span(0, 1_000), 26).unwrap();

        // The reader fell behind and sought to the live edge.
        assert_eq!(l.skip_to(t(5_000)).unwrap(), Discontinuity::GAP);
        assert_eq!(l.read(span(5_000, 6_000)).unwrap(), Discontinuity::NONE);
        l.credit(span(5_000, 6_000), 26).unwrap();

        // The skipped 4 000 ns yields nothing, and cannot be claimed.
        assert_eq!(l.credit(span(1_000, 5_000), 104), Err(LedgerError::NotRead));
        assert_eq!(l.credit(span(900, 5_100), 104), Err(LedgerError::NotRead));
        assert_eq!(l.evidence_bits(), 52);
        assert_eq!(l.covered_ns(), 2_000);
        assert_eq!(l.skipped_ns(), 4_000);
        assert_eq!(l.skips(), 1);
    }

    /// A gapless-RDS run is Rule I — a second pipeline over the ring — and its evidence is
    /// likewise bounded by what it read.
    #[test]
    fn a_region_job_credits_only_what_it_read() {
        let mut j = job(0, 1_000);
        j.complete(span(0, 600)).unwrap();
        assert_eq!(j.credit(span(0, 600), 64), Ok(()));
        assert_eq!(
            j.credit(span(0, 1_000), 64),
            Err(RegionError::Ledger(LedgerError::NotRead))
        );
        assert_eq!(j.coverage().evidence_bits(), 64);
    }

    /// ADR-0017 §6.3: handover is an explicit named state, and carries a DISCONTINUITY when
    /// anything fell between the two readers.
    #[test]
    fn handover_is_named_and_carries_its_discontinuity() {
        let mut j = job(0, 1_000);
        j.complete(span(0, 1_000)).unwrap();
        assert_eq!(j.phase(), RegionPhase::Region);

        let h = j.hand_over(t(1_400)).unwrap();
        assert_eq!(h.at, t(1_000));
        assert_eq!(h.live_from, t(1_400));
        assert_eq!(h.gap, Some(span(1_000, 1_400)));
        assert_eq!(h.flags, Discontinuity::GAP);
        assert!(!h.is_seamless());
        assert_eq!(
            j.phase(),
            RegionPhase::HandedOver {
                at: t(1_000),
                live_from: t(1_400)
            }
        );
    }

    /// A handover that loses nothing is seamless — and still an explicit state, never a merge.
    #[test]
    fn a_seamless_handover_is_still_a_state() {
        let mut j = job(0, 1_000);
        j.complete(span(0, 1_000)).unwrap();
        let h = j.hand_over(t(1_000)).unwrap();
        assert!(h.is_seamless());
        assert_eq!(h.gap, None);
        assert_eq!(h.abandoned_ns, 0);
        assert!(matches!(j.phase(), RegionPhase::HandedOver { .. }));
    }

    /// Enqueued-but-unread work at a handover is a hole in the coverage bar, named rather than
    /// rounded away — and the job is over, so the samples cannot be claimed later.
    #[test]
    fn handover_with_work_outstanding_is_a_hole() {
        let mut j = job(0, 1_000);
        j.complete(span(0, 1_000)).unwrap();
        j.extend_to(t(3_000)).unwrap().unwrap();

        let h = j.hand_over(t(3_000)).unwrap();
        assert_eq!(h.abandoned_ns, 2_000);
        assert_eq!(h.at, t(1_000));
        assert_eq!(h.gap, Some(span(1_000, 3_000)));
        assert_eq!(h.flags, Discontinuity::GAP);
        assert_eq!(j.coverage().covered_ns(), 1_000);

        assert_eq!(j.extend_to(t(4_000)), Err(RegionError::HandedOver));
        assert_eq!(j.complete(span(1_000, 2_000)), Err(RegionError::HandedOver));
        assert_eq!(j.hand_over(t(4_000)), Err(RegionError::HandedOver));
    }

    /// The §6.3 shape: a growing region job and a live listener over the same emitter are two
    /// readers, two policies and two ledgers. Nothing here fuses them.
    #[test]
    fn the_region_job_and_the_listener_are_two_readers() {
        let listener = PipelinePlan::live_edge(600_000_000).with_output(OutputRole::Audio);
        let mut j = job(0, 1_000);
        listener.validate().unwrap();

        assert_eq!(j.policy(), ReaderPolicy::BoundedRegion);
        assert!(listener.policy().is_live_edge());
        assert_ne!(j.policy(), listener.policy());
        assert_eq!(j.policy().name(), "bounded-region");
        assert_eq!(listener.policy().name(), "live-edge");

        // The listener's skip does not touch the region job's coverage, and the region job's
        // backlog does not reach the listener.
        let mut audio = ReadLedger::new(listener.policy());
        audio.read(span(0, 100)).unwrap();
        audio.skip_to(t(5_000)).unwrap();
        j.complete(span(0, 1_000)).unwrap();
        assert_eq!(j.coverage().skipped_ns(), 0);
        assert_eq!(j.coverage().covered_ns(), 1_000);
        assert_eq!(audio.skipped_ns(), 4_900);
    }

    /// A live-edge reader that has read nothing yet is not "behind": attaching is not a skip.
    #[test]
    fn attaching_at_the_live_edge_is_not_a_skip() {
        let mut l = ReadLedger::new(ReaderPolicy::live_edge_secs(0.6));
        assert_eq!(l.skip_to(t(9_000)).unwrap(), Discontinuity::STREAM_START);
        assert_eq!(l.skips(), 0);
        assert_eq!(l.skipped_ns(), 0);
        assert_eq!(l.read(span(9_000, 9_500)).unwrap(), Discontinuity::NONE);
        assert_eq!(l.covered_ns(), 500);
        assert_eq!(l.skip_to(t(9_500)).unwrap(), Discontinuity::NONE);
        assert_eq!(l.skip_to(t(9_000)), Err(LedgerError::SkipBackwards));
    }

    #[test]
    fn inverted_extents_are_refused() {
        let bad = Region::new(FreqRange::new(1.0, 2.0), span(1_000, 0));
        assert_eq!(RegionJob::open(bad).err(), Some(RegionError::Inverted));
        let mut l = ReadLedger::new(ReaderPolicy::BoundedRegion);
        assert_eq!(l.read(span(10, 0)), Err(LedgerError::Inverted));
        assert_eq!(l.credit(span(10, 0), 1), Err(LedgerError::Inverted));
    }

    #[test]
    fn errors_render() {
        assert!(
            PlanError::AudioOnBoundedRegion
                .to_string()
                .contains("audio")
        );
        assert!(LedgerError::SkipForbidden.to_string().contains("skip"));
        assert!(
            RegionError::from(LedgerError::NotRead)
                .to_string()
                .contains("evidence")
        );
        assert!(RegionError::HandedOver.to_string().contains("handed over"));
        assert!(RegionError::NoWork.to_string().contains("no work"));
        assert!(RegionError::OutsideWork.to_string().contains("outside"));
        assert!(RegionError::Inverted.to_string().contains("before"));
        assert!(LedgerError::Inverted.to_string().contains("before"));
        assert!(LedgerError::AlreadyRead.to_string().contains("twice"));
        assert!(LedgerError::SkipBackwards.to_string().contains("backwards"));
        assert!(
            PlanError::MultipleAudioOutputs
                .to_string()
                .contains("one audio")
        );
    }
}
