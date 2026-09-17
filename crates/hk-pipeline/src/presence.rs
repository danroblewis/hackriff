//! The live presence-**endpoint** stream (T-410, ADR-0019; ADR-0004 §15 / `docs/stream-contract.md`
//! §15): the `presence` stream, one record when an emitter's presence interval **opens**, and one
//! when it **closes**. Nothing at all while it continues.
//!
//! # The contract, and the one it replaces
//!
//! T-388 built this stream under **contract A — presence is an accumulation of observations**. The
//! box's top was the newest *measured* end, and a record per open emitter per tick pushed it
//! forward. It could never over-claim, and it could never say the one thing a live spectrum display
//! exists to say — *this signal is on the air now* — because "now" is always after the last
//! measurement.
//!
//! The user replaced it with **contract B — presence is an interval with endpoints** (ADR-0019).
//! The box runs from its start **straight to the live edge** and caps only on a real detected end,
//! so the measurement is the START event plus the **absence of an END**. That is what tracking is:
//! a track is open until it closes. The stream therefore needs only
//! [`START`](PresenceEventKind::Start) / [`END`](PresenceEventKind::End) /
//! [`REOPEN`](PresenceEventKind::Reopen) — **never a per-poll presence bump to advance the box
//! top**, which the user explicitly rejected, along with rapidly re-testing presence and gating the
//! next waterfall row on an end-decision.
//!
//! A continuing interval emits **nothing**. Over a band of steady broadcast carriers this stream
//! goes silent, where contract A emitted one record per emitter per tick forever.
//!
//! # Where the honesty burden went
//!
//! Under contract A a late end-decision cost nothing: the box simply stopped growing. Under
//! contract B **a late or missed END means the box over-claims silent air, all the way to the live
//! edge.** Two things carry that, and both are stated rather than assumed:
//!
//! 1. **The renderer draws the assumption as an assumption.** The span from the last *measured*
//!    end to the live edge is the box's open cap and is drawn distinctly (`ui/src/timebox.ts`), so
//!    the display always shows where measurement stops — and the cap grows visibly as the silence
//!    grows, which is how a suspected end is legible without being acted on.
//! 2. **The END carries the *measured* end, not the decision instant** ([`PresenceEvent::t_end_ns`]
//!    is `LiveExtent::t_end_ns`, the end of the last burst the detector saw). So when it fires the
//!    box does not stop where the assumption had reached: it **retracts** to where measurement
//!    actually stopped, and the over-claim is transient.
//!
//! # The end detector
//!
//! An interval closes after one idle gap of **observed** silence — the rule `hk_model::presence`
//! already owns, with the reason it already owns: *a gap shorter than the revisit period is not
//! evidence of absence, because the receiver was not listening.* [`LiveExtent`] now carries that
//! silence in two clocks, so [`interval_closed`] can tell the two cases apart:
//!
//! | the receiver | gap used | closes after |
//! |---|---|---|
//! | never looked away (observed silence accounts for the wall silence) | [`MIN_IDLE_GAP_S`] | **1 s**, plus ≤ one tick |
//! | looked away (a sweep, a retune) | [`MAX_IDLE_GAP_S`] | 60 observed s — in practice the 5 s inventory poll closes it first, which is the safe direction |
//!
//! So on the case that matters — a live dwell, which is the whole of Explore — the stream and the
//! 5 s poll close at the same instant, because both are `now − t_end > 1 s` on the same timestamps.
//! On a sweep the poll closes first and the stream never holds a box open longer than the poll
//! would. **The stream can be late; it can never be the reason a box stays open.**
//!
//! Latency: **≤ 1.25 s** under a dwell (1 s gap + ≤ 250 ms tick). Missed END: the inventory poll is
//! the backstop, so **≤ 5 s** — and under contract B the poll is permitted to *cap* a box, where
//! under contract A both surfaces could only ever extend.
//!
//! # The END is provisional: [`REVOKE`](PresenceEventKind::Revoke) (T-413)
//!
//! *(The user, 2026-09-17, answering the question ADR-0019 §6.1 flagged.)* A detected end is
//! **revocable**: a signal that resumes within tolerance nulls the end and **keeps the one interval
//! open** on the **same row**, rather than splitting it or spawning an emitter. The tolerance is the
//! **existing idle gap** — no new parameter.
//!
//! **The window is anchored on the END event, not on the measured end.** An interval closes only
//! after a full gap of observed silence, so at the instant an END exists the silence since the
//! *measured* end is already exactly one gap: a window measured from there could never be reached.
//! It therefore runs from the **decision** and lasts one gap — which places its far edge at
//! `measured end + 2 × gap`, and that is the form it is written in
//! ([`Announced::revocable_until_ns`], [`hk_model::IdleGap::revocable_nanos`]). Writing it off the
//! measurement rather than off the decision is what keeps this stream and the 5 s poll the **same
//! predicate on the same timestamps** (ADR-0019 §4), and stops the window silently widening when an
//! END was published late.
//!
//! **So the effective join tolerance is `2 × gap` although only one constant exists**, and it is not
//! that constant doubled: the first gap is the observed absence that *justifies* the end, the second
//! the observed absence that *confirms* it. Equivalently, the end stands revocable for exactly as
//! long as `hk_model::confidence_after_silence` is still above `1/e`.
//!
//! **What makes a resumption visible here.** A capped interval is kept, not forgotten: on a later
//! tick its extent's measured end has moved past the end this stream published, which is the whole
//! signal — the resumption's own *start* is never in the extent (it carries `t_first`), and nothing
//! here needs it, because the REVOKE re-opens the interval the END capped rather than opening a new
//! one. One gap later with no resumption, the END becomes final and the entry is forgotten, after
//! which the same track returning publishes nothing and the poll serves it (ADR-0019 §6.1,
//! unchanged).
//!
//! **What the END now means to a consumer.** It is a cap that may be withdrawn for one idle gap, so
//! a consumer must not treat it as final before then. That is a change to an existing record's
//! meaning and not only an added kind, which is why the schema goes to `/3` rather than pretending
//! the addition is transparent.
//!
//! # Shape
//!
//! `messages` kind, schema [`PRESENCE_MESSAGE_SCHEMA`], `content_class` unrestricted:
//!
//! ```json
//! {"type":"message","seq":7,"t_ns":1757774400123456789,"emitter_id":"0199…",
//!  "content_class":"unrestricted","gated":false,"frame_model":"presence-end",
//!  "metadata":{"kind":"presence-end",
//!              "last_interval":{"t_start_s":1757774390.1,"t_end_s":1757774400.12,"open":false}}}
//! ```
//!
//! The envelope's `t_ns` is the instant the record is *about* — the interval's start for an opening
//! record, its measured end for a closing one (T-354: integer Unix nanoseconds, never a bare `t`).
//! `metadata.last_interval` is deliberately the **same object** `GET /api/inventory` serves as
//! `presence.last_interval`, so a client assigns it rather than rebuilding it and the fast surface
//! cannot invent a shape the slow one would disagree with.
//!
//! **No frequency.** An endpoint is new *time*, not new geometry (T-362: a `TimeBox` is a band
//! fraction plus two absolute capture times). This path can never move a box sideways.
//!
//! # Rate, and what bounds it
//!
//! Unchanged from T-388: at most one tick per [`PRESENCE_PUSH_NS`], at most
//! [`MAX_EVENTS_PER_TICK`] records in a tick, a hard 128 records/s ceiling however busy the band
//! is, because the cap is on records and not on tracks. **The truncation policy is what changed.**
//! Under contract A a record left out of a tick cost only freshness. Under contract B a dropped END
//! costs an over-claim of silent air — so within a tick **ENDs go first**, and what does not fit is
//! **carried to the next tick** rather than discarded (`presence_events_deferred`). A slow consumer
//! is still dropped by the publisher's own policy (ADR-0004 §7), never the survey; that is the case
//! the inventory poll backstops.

use std::collections::{HashMap, VecDeque};

use hk_detect::LiveExtent;
use hk_model::{ContentClass, EmitterId, MAX_IDLE_GAP_S, MIN_IDLE_GAP_S, Timestamp, TrackId};
use hk_stream::{MessageRecord, Publisher, PublisherConfig, StreamHeader, StreamKind};
use serde_json::json;

use crate::config::StreamSink;

/// Stream id of the presence-endpoint stream (`/ws/presence`).
pub const PRESENCE_STREAM_ID: &str = "presence";
/// Message schema. **Bumped to `/2` by T-410**: the record kinds changed, and a consumer that only
/// understands `presence-extension` must see a schema it does not know rather than silently ignore
/// every endpoint on the stream. **Bumped to `/3` by T-413**: `presence-revoke` is added, and — the
/// part that is not additive — `presence-end` becomes *provisional* for one idle gap, so a consumer
/// that files an END as final is now wrong about a record it already understands.
pub const PRESENCE_MESSAGE_SCHEMA: &str = "hackriff.presence/3";

/// `metadata.kind` (and `frame_model`) of a record opening an emitter's **first** interval.
pub const PRESENCE_START_KIND: &str = "presence-start";
/// …opening a **later** interval on an emitter that has been on the air before.
pub const PRESENCE_REOPEN_KIND: &str = "presence-reopen";
/// …closing an interval, at its **measured** end.
pub const PRESENCE_END_KIND: &str = "presence-end";
/// …**withdrawing** an END published within the last idle gap: the same interval, still open
/// (T-413). Never a new interval — that is [`PRESENCE_REOPEN_KIND`], and it draws a second box.
pub const PRESENCE_REVOKE_KIND: &str = "presence-revoke";

/// Shortest gap between ticks, stream ns. 250 ms: well inside the ~1 s the box must cap within, and
/// four times the 0.5 s detect flush that feeds it, so the flush — not this — paces the stream.
pub const PRESENCE_PUSH_NS: i64 = 250_000_000;

/// Records one tick may publish. The rate ceiling (see the module docs); not a claim about how many
/// emitters are on the air.
pub const MAX_EVENTS_PER_TICK: usize = 32;

/// Deferred events held for a later tick before the oldest are dropped. Generous, because under
/// contract B a dropped END is an over-claim and the queue only fills when more than 128 endpoints
/// a second are happening — at which point the 5 s poll is the honest backstop anyway.
pub const MAX_DEFERRED_EVENTS: usize = 1024;

/// Emitters remembered as having been on the air, for telling a REOPEN from a START. Bounded: a
/// forgotten emitter's return is labelled `presence-start` instead of `presence-reopen`, which
/// **renders identically** (both open a new box) — the kind is a hint for a consumer that wants to
/// know a returning emitter from a new one without re-querying, never the thing the box is built
/// from.
const MAX_REMEMBERED: usize = 4096;

/// Slack when comparing observed silence with wall silence, ns. The tracker's own
/// `coverage_slack_s` (2 ms): spans closer together than this are one contiguous observation, so
/// two figures within it of each other describe a receiver that never looked away.
const COVERAGE_SLACK_NS: i64 = 2_000_000;

const NS_PER_S: f64 = 1e9;

/// Whether this extent's interval has closed — the end detector (module docs, ADR-0019 §3).
///
/// The receiver never looked away exactly when the silence it *observed* accounts for the silence
/// on the wall clock; only then may the interval close at the [`MIN_IDLE_GAP_S`] floor, which is
/// the tracker's own `max_transition_gap_s` and the shortest silence anything in this codebase is
/// allowed to read as the end of an emission. Otherwise the revisit period is not known here and
/// the conservative gap applies — this stream defers, and the inventory poll (which measures the
/// gap off the run's tune history, `hk_api::coverage::ObservedCoverage`) closes the box.
pub fn interval_closed(e: &LiveExtent) -> bool {
    e.observed_silence_ns > idle_gap_ns(e)
}

/// The idle gap this extent's endpoints are judged under, ns — the gap that closes its interval and,
/// once closed, the gap for which the END stands **revocable** (T-413). One derivation, so the two
/// decisions can never be taken under different numbers.
pub fn idle_gap_ns(e: &LiveExtent) -> i64 {
    let watched = e.observed_silence_ns >= e.wall_silence_ns.saturating_sub(COVERAGE_SLACK_NS);
    let gap_s = if watched {
        MIN_IDLE_GAP_S
    } else {
        MAX_IDLE_GAP_S
    };
    (gap_s * NS_PER_S) as i64
}

/// Which endpoint a record announces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresenceEventKind {
    /// A first interval opened on this emitter.
    Start,
    /// A later interval opened on an emitter that has been on the air before — after a silence
    /// longer than the idle gap, which is what makes it a *new* interval rather than a
    /// continuation (ADR-0019 §6).
    Reopen,
    /// An interval closed, at its measured end. **Provisional**: revocable for one idle gap
    /// (T-413).
    End,
    /// An END published within the last idle gap is **withdrawn**: the signal came back inside the
    /// revocation window, so the interval it capped is the *same* interval and is open again
    /// (T-413). It carries that interval's original `t_start_s`, which is what distinguishes it
    /// from a [`Reopen`](Self::Reopen): one box grows, rather than a second appearing.
    Revoke,
}

impl PresenceEventKind {
    /// The wire `kind` / `frame_model`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => PRESENCE_START_KIND,
            Self::Reopen => PRESENCE_REOPEN_KIND,
            Self::End => PRESENCE_END_KIND,
            Self::Revoke => PRESENCE_REVOKE_KIND,
        }
    }

    /// Whether this kind leaves the interval open (and so the box running to the live edge).
    pub const fn open(self) -> bool {
        !matches!(self, Self::End)
    }
}

/// One endpoint as the wire sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresenceEvent {
    /// Which endpoint.
    pub kind: PresenceEventKind,
    /// The emitter whose row the client is holding.
    pub emitter: EmitterId,
    /// Start of the interval, Unix ns.
    pub t_start_ns: i64,
    /// **Measured** end of the interval, Unix ns — the last burst end the detector saw, never the
    /// instant the end was decided and never `now`. On an opening record it is the interval's start
    /// so far; on a closing one it is where the box retracts to.
    pub t_end_ns: i64,
    /// Measured silence inside this interval whose end was revoked, ns (T-413). 0 on every kind but
    /// [`Revoke`](PresenceEventKind::Revoke).
    ///
    /// A **lower bound**, and a measured one: it is the silence the receiver actually watched and
    /// acted on — from the capped measured end to the tick that published the END, one full idle
    /// gap. The true gap runs on to the resumption's first sample, which is not in a
    /// [`LiveExtent`] (it carries `t_first`), so the stream states what it can show and the
    /// inventory poll replaces it with the exact figure within 5 s. Nothing computes air time from
    /// this; it is what tells a viewer the box covers silence at all.
    pub revoked_ns: i64,
}

/// Unix seconds of a nanosecond instant, the `_s` unit `docs/api.md` and the inventory row use.
fn secs(ns: i64) -> f64 {
    ns as f64 * 1e-9
}

impl PresenceEvent {
    /// The record this event publishes as. Pure, so the wire shape is unit-tested without a
    /// publisher (see the tests at the foot of this module).
    pub fn record(&self) -> MessageRecord {
        // The instant the record is *about*: where a box's newest edge lands because of it. An
        // opening record places a box's oldest edge; an END caps one at its measured end, and a
        // REVOKE re-opens that same box with its measured edge where the resumption has reached.
        let t = match self.kind {
            PresenceEventKind::Start | PresenceEventKind::Reopen => self.t_start_ns,
            PresenceEventKind::End | PresenceEventKind::Revoke => self.t_end_ns,
        };
        MessageRecord {
            t: Timestamp::from_unix_nanos(t),
            emitter_id: Some(self.emitter),
            provenance_ref: None,
            // Timing of exactly the class `/api/inventory`'s own `presence` object carries
            // unconditionally (`hk_api::presence`: serving it uniformly is what keeps its presence
            // from signalling that a row's identity was withheld). No identity, no content.
            content_class: ContentClass::Unrestricted,
            decode_id: None,
            annotation_id: None,
            decoder: None,
            frame_model: Some(self.kind.as_str().into()),
            crc_status: None,
            identity: None,
            metadata: json!({
                "kind": self.kind.as_str(),
                // The `presence.last_interval` object verbatim (docs/api.md), so the client assigns
                // it instead of rebuilding it. `open` is stated here, never inferred there.
                "last_interval": {
                    "t_start_s": secs(self.t_start_ns),
                    "t_end_s": secs(self.t_end_ns),
                    "open": self.kind.open(),
                    "revoked_s": secs(self.revoked_ns),
                },
            }),
            content: None,
        }
    }
}

/// An interval this stream has announced, and has not yet finished with: either still open, or
/// capped by an END that is **still revocable** (T-413).
#[derive(Clone, Copy, Debug)]
struct Announced {
    track: TrackId,
    t_start_ns: i64,
    t_end_ns: i64,
    /// The tick at which this stream published the END capping this interval, capture ns. `None`
    /// while the interval is open. Its only use is the **lower bound** a REVOKE reports as the
    /// silence measured ([`PresenceEvent::revoked_ns`]); the window itself is measured off
    /// [`Self::t_end_ns`], for the reason on [`Self::revocable_until_ns`].
    capped_at_ns: Option<i64>,
    /// The idle gap the END was decided under ([`idle_gap_ns`]) — and therefore the length of the
    /// revocation window, so the detection and its confirmation are one number, measured once.
    gap_ns: i64,
}

impl Announced {
    /// The last instant a resumption may begin and still revoke this interval's END, capture ns.
    ///
    /// The user's rule is "within one idle gap **of the detected end**", and the detected end is
    /// the END *event* — a window measured from the *measured* end could never be reached, because
    /// an END only fires once a full gap of silence has already been observed past it. That makes
    /// the window `[measured end + gap, measured end + 2 × gap]`, and it is implemented in that
    /// second form — off the measurement rather than off the decision — for two reasons: it is
    /// literally the predicate [`hk_model::IdleGap::revocable_nanos`] states, so the stream and the
    /// 5 s poll cannot disagree about which resumptions are one interval; and it does not drift if
    /// the END itself was published late, where anchoring on the decision would silently widen.
    fn revocable_until_ns(&self) -> i64 {
        // Clamped by `MAX_IDLE_GAP_S` for the reason `IdleGap::revocable_nanos` is: past the
        // tracker's own idle timeout the discontinuity was judged by a measurement upstream, and
        // revocation may not overrule it (ADR-0019 §6). It binds only on the unknown-revisit path.
        let window = self
            .gap_ns
            .saturating_mul(2)
            .min((MAX_IDLE_GAP_S * NS_PER_S) as i64);
        self.t_end_ns.saturating_add(window)
    }

    /// Whether this entry's END has been published and is no longer revocable at `now_ns`.
    ///
    /// One [`PRESENCE_PUSH_NS`] of slack, because a resumption is only ever *noticed* at a tick:
    /// without it, a signal that came back inside the window would have its revocation refused for
    /// having been reported on time.
    fn end_is_final(&self, now_ns: i64) -> bool {
        self.capped_at_ns.is_some()
            && now_ns > self.revocable_until_ns().saturating_add(PRESENCE_PUSH_NS)
    }
}

/// The run's presence-endpoint publisher: the tick gate, the open set, and the stream itself.
///
/// It holds no track→emitter map of its own. The join is asked of the inventory
/// ([`crate::Inventory::emitter_of_track`]) at each tick, which is the one place that knows it and
/// the one place that stays right through a merge or a link — a second copy here would be a second
/// opinion about which box a signal is.
pub struct PresenceStream {
    publisher: Option<Publisher>,
    /// Stream time of the last tick.
    last_ns: Option<i64>,
    /// Emitters whose interval this stream has opened and not yet *finished with* — open, or capped
    /// by an END that is still revocable (T-413).
    open: HashMap<EmitterId, Announced>,
    /// Emitters this stream has closed an interval for, newest last — the REOPEN/START test.
    remembered: VecDeque<EmitterId>,
    /// **Tracks** this stream has published an END for, newest last.
    ///
    /// A track outlives the interval this stream capped: the tracker joins bursts across its own
    /// `idle_timeout_s` (60 observed s), far past the idle gap a *box* caps at. So the same track
    /// reappears, un-silent, after its END — and its extent still carries `t_first`, **the track's
    /// first burst**, not the resumption. Publishing that as a new interval's start would claim the
    /// very silence the END was just drawn for. The resumption's own start is not in the extent, so
    /// there is no honest opening record to publish, and this stream says nothing: the next
    /// inventory poll serves the new interval with the start it actually has (ADR-0019 §6).
    ended_tracks: VecDeque<TrackId>,
    /// Events the per-tick cap could not fit, ENDs first (module docs).
    deferred: VecDeque<PresenceEvent>,
}

impl PresenceStream {
    /// Offers the `presence` stream through `sink`, when there is one.
    pub(crate) fn new(sink: Option<&StreamSink>) -> anyhow::Result<Self> {
        let publisher = match sink {
            Some(sink) => {
                let mut header = StreamHeader::new(
                    PRESENCE_STREAM_ID,
                    StreamKind::Messages,
                    // Timing metadata (when an emitter started and stopped), never RF content.
                    ContentClass::Unrestricted,
                    format!("hk-pipeline:presence@{}", env!("CARGO_PKG_VERSION")),
                );
                header.message_schema = Some(PRESENCE_MESSAGE_SCHEMA.into());
                header.max_frame_len = 16 * 1024;
                let publisher = Publisher::new(header.clone(), PublisherConfig::default())?;
                sink(&header, publisher.handle());
                Some(publisher)
            }
            None => None,
        };
        Ok(Self {
            publisher,
            last_ns: None,
            open: HashMap::new(),
            remembered: VecDeque::new(),
            ended_tracks: VecDeque::new(),
            deferred: VecDeque::new(),
        })
    }

    /// Whether `now_ns` is far enough past the last tick to publish again.
    pub fn due(&self, now_ns: i64) -> bool {
        self.last_ns
            .is_none_or(|t| now_ns.saturating_sub(t) >= PRESENCE_PUSH_NS)
    }

    /// Whether anything is open or deferred — so a tick with **no extents at all** still runs, and
    /// the last emission on the air still gets its END. (Under contract A an empty extent list was
    /// nothing to say; under contract B it is "everything stopped", which is the most important
    /// thing the stream ever says.)
    pub fn has_state(&self) -> bool {
        !self.open.is_empty() || !self.deferred.is_empty()
    }

    /// Records that `e`'s interval on `track` has been closed.
    fn remember(&mut self, e: EmitterId, track: TrackId) {
        self.remembered.push_back(e);
        while self.remembered.len() > MAX_REMEMBERED {
            self.remembered.pop_front();
        }
        self.ended_tracks.push_back(track);
        while self.ended_tracks.len() > MAX_REMEMBERED {
            self.ended_tracks.pop_front();
        }
    }

    /// START or REOPEN, by whether this stream has closed an interval for the emitter before
    /// (ADR-0019 §6: a reopen binds to the same emitter, a start need not).
    fn opening_kind(&self, e: EmitterId) -> PresenceEventKind {
        if self.remembered.contains(&e) {
            PresenceEventKind::Reopen
        } else {
            PresenceEventKind::Start
        }
    }

    /// The endpoints `extents` implies, given what this stream has already announced.
    ///
    /// `extents_complete` says the caller's list is the whole live set. When it is not — a
    /// truncated batch — an **absent** extent is not read as a closed interval, because the track
    /// may simply have been cut from the list. A stale open box is corrected by the inventory poll;
    /// a fabricated END would cap a box on air.
    pub fn plan(
        &mut self,
        now_ns: i64,
        extents: &[LiveExtent],
        extents_complete: bool,
        emitter_of: impl Fn(TrackId) -> Option<EmitterId>,
    ) -> Vec<PresenceEvent> {
        let mut ends: Vec<PresenceEvent> = Vec::new();
        let mut opens: Vec<PresenceEvent> = Vec::new();
        let mut seen: Vec<EmitterId> = Vec::with_capacity(extents.len());

        // An END whose revocation window has run out is final. Retiring these first — on the tick
        // clock, so it happens whether or not an extent is still being offered for the track — is
        // what stops a capped entry lingering and stops the vanished-track sweep below ending an
        // interval it already ended (T-413).
        let expired: Vec<(EmitterId, TrackId)> = self
            .open
            .iter()
            .filter(|(_, a)| a.end_is_final(now_ns))
            .map(|(e, a)| (*e, a.track))
            .collect();
        for (emitter, track) in expired {
            self.open.remove(&emitter);
            self.remember(emitter, track);
        }

        for e in extents {
            // An extent for a track the inventory has given no row is not addressed to anything:
            // there is no box on screen, and inventing an id would address one that does not exist.
            let Some(emitter) = emitter_of(e.track) else {
                continue;
            };
            seen.push(emitter);
            let closed = interval_closed(e);
            match self.open.get(&emitter).copied() {
                // **The END was provisional, and this is the resumption that revokes it** (T-413).
                // The interval the END capped re-opens — same emitter, same row, same `t_start_ns`,
                // ONE interval — rather than a REOPEN's second box. A capped entry that is still
                // silent says nothing; one whose window has run out was already retired above.
                Some(a) if a.capped_at_ns.is_some() => {
                    // The resumption's own start when the extent has one past the cap (a new track
                    // bound to this emitter): then the gap is *known*, and is checked against the
                    // same `2 × gap` the batch derivation uses. Otherwise the same track came back
                    // and its `t_start_ns` is `t_first`, so the tick that noticed it is the best
                    // clock there is, with one tick of slack for the noticing.
                    let resumed_at = (e.t_start_ns > a.t_end_ns).then_some(e.t_start_ns);
                    let inside = match resumed_at {
                        Some(t) => t <= a.revocable_until_ns(),
                        None => {
                            e.t_end_ns > a.t_end_ns
                                && now_ns <= a.revocable_until_ns() + PRESENCE_PUSH_NS
                        }
                    };
                    if inside {
                        let capped_at = a.capped_at_ns.expect("guarded by the arm");
                        opens.push(PresenceEvent {
                            kind: PresenceEventKind::Revoke,
                            emitter,
                            t_start_ns: a.t_start_ns,
                            t_end_ns: e.t_end_ns.max(a.t_end_ns),
                            // What the receiver watched and acted on, exactly when the gap is
                            // known; otherwise the silence up to the END it is withdrawing, which
                            // is the part it can show. The poll states the full figure.
                            revoked_ns: resumed_at
                                .unwrap_or(capped_at)
                                .saturating_sub(a.t_end_ns)
                                .max(0),
                        });
                        self.open.insert(
                            emitter,
                            Announced {
                                track: e.track,
                                t_start_ns: a.t_start_ns,
                                t_end_ns: e.t_end_ns.max(a.t_end_ns),
                                capped_at_ns: None,
                                gap_ns: a.gap_ns,
                            },
                        );
                    } else if let Some(t) = resumed_at {
                        // Past the window, and this extent's start is the resumption's *own*: the
                        // END stands and this is a genuinely new interval. ADR-0019 §6.1 leaves the
                        // same *track*'s return to the poll because its only available start is
                        // `t_first`; a start on the near side of the capped silence is honest, and
                        // publishing it here is what stops a refused revocation stalling the box
                        // until the poll catches up.
                        self.open.remove(&emitter);
                        self.remember(emitter, a.track);
                        if !closed {
                            opens.push(PresenceEvent {
                                kind: self.opening_kind(emitter),
                                emitter,
                                t_start_ns: t,
                                t_end_ns: e.t_end_ns,
                                revoked_ns: 0,
                            });
                            self.open.insert(
                                emitter,
                                Announced {
                                    track: e.track,
                                    t_start_ns: t,
                                    t_end_ns: e.t_end_ns,
                                    capped_at_ns: None,
                                    gap_ns: idle_gap_ns(e),
                                },
                            );
                        }
                    }
                }
                // A different, later interval on the same emitter, across a silence too long to be
                // one interval: the announced one ended where it was last measured, and this one
                // opens. Emitting both keeps the silence between them drawn as silence — a REOPEN
                // never stretches a box across it (ADR-0019 §6).
                //
                // The guard is T-413's: inside `2 × gap` there is no second interval at all. No END
                // was ever published for this entry (it is still open — the resumption beat the tick
                // that would have capped it), so there is nothing to revoke and nothing to say: the
                // interval simply continues, and the poll records the silence it crossed as a
                // revoked gap. Falling through to the arm below is exactly that.
                Some(a) if e.t_start_ns > a.t_start_ns && e.t_start_ns > a.revocable_until_ns() => {
                    ends.push(PresenceEvent {
                        kind: PresenceEventKind::End,
                        emitter,
                        t_start_ns: a.t_start_ns,
                        t_end_ns: a.t_end_ns,
                        revoked_ns: 0,
                    });
                    self.open.remove(&emitter);
                    self.remember(emitter, e.track);
                    if !closed {
                        opens.push(PresenceEvent {
                            kind: PresenceEventKind::Reopen,
                            emitter,
                            t_start_ns: e.t_start_ns,
                            t_end_ns: e.t_end_ns,
                            revoked_ns: 0,
                        });
                        self.open.insert(
                            emitter,
                            Announced {
                                track: e.track,
                                t_start_ns: e.t_start_ns,
                                t_end_ns: e.t_end_ns,
                                capped_at_ns: None,
                                gap_ns: idle_gap_ns(e),
                            },
                        );
                    }
                }
                // Already announced open. The measured end moves silently — that is the whole point
                // of contract B, and the reason there is no per-tick record here.
                Some(a) => {
                    if closed {
                        ends.push(PresenceEvent {
                            kind: PresenceEventKind::End,
                            emitter,
                            t_start_ns: a.t_start_ns,
                            // The *measured* end, taken from the extent rather than from what was
                            // announced, so the box retracts to where the detector last heard it.
                            t_end_ns: e.t_end_ns.max(a.t_start_ns),
                            revoked_ns: 0,
                        });
                        // **Kept, not forgotten** (T-413): the END is provisional for one further
                        // gap, and only an entry that is still here can be revoked. `end_is_final`
                        // retires it when the window runs out, and until then the emitter is not
                        // `remember`ed — so a resumption inside the window revokes, and one outside
                        // it takes the unchanged ADR-0019 §6.1 route through the poll.
                        self.open.insert(
                            emitter,
                            Announced {
                                track: e.track,
                                t_start_ns: a.t_start_ns,
                                t_end_ns: e.t_end_ns.max(a.t_start_ns),
                                capped_at_ns: Some(now_ns),
                                gap_ns: idle_gap_ns(e),
                            },
                        );
                    } else if e.t_end_ns > a.t_end_ns {
                        self.open.insert(
                            emitter,
                            Announced {
                                track: e.track,
                                t_start_ns: a.t_start_ns,
                                t_end_ns: e.t_end_ns,
                                capped_at_ns: None,
                                gap_ns: idle_gap_ns(e),
                            },
                        );
                    }
                }
                // Not announced, and this track has already had an interval capped: its extent's
                // start is the track's first burst, not this resumption, so there is no honest
                // opening record to publish (see `ended_tracks`). The poll serves it.
                None if self.ended_tracks.contains(&e.track) => {}
                // Not announced. An interval that is *already* closed opens nothing: its box is the
                // inventory poll's business, and announcing an open interval only to close it in
                // the same breath would flash a box to the live edge for no measured reason.
                None if !closed => {
                    opens.push(PresenceEvent {
                        kind: self.opening_kind(emitter),
                        emitter,
                        t_start_ns: e.t_start_ns,
                        t_end_ns: e.t_end_ns,
                        revoked_ns: 0,
                    });
                    self.open.insert(
                        emitter,
                        Announced {
                            track: e.track,
                            t_start_ns: e.t_start_ns,
                            t_end_ns: e.t_end_ns,
                            capped_at_ns: None,
                            gap_ns: idle_gap_ns(e),
                        },
                    );
                }
                None => {}
            }
        }

        // The belt-and-braces END: an announced emitter with no extent at all. Its track closed
        // (idle, capacity, end of stream), or its row was unbound. Either way the box must cap, and
        // this is what makes "there is no path that produces no END" true.
        // An entry already capped is skipped: its END has been published, it is waiting out its
        // revocation window, and ending it again would publish a duplicate (T-413).
        if extents_complete {
            let gone: Vec<EmitterId> = self
                .open
                .iter()
                .filter(|(e, a)| !seen.contains(e) && a.capped_at_ns.is_none())
                .map(|(e, _)| *e)
                .collect();
            for emitter in gone {
                let a = self.open.get_mut(&emitter).expect("key came from the map");
                let ev = PresenceEvent {
                    kind: PresenceEventKind::End,
                    emitter,
                    t_start_ns: a.t_start_ns,
                    t_end_ns: a.t_end_ns,
                    revoked_ns: 0,
                };
                // Capped, not dropped: a track that vanished for a moment — a truncated batch it
                // was cut from, a momentary unbinding — and comes back inside the window revokes
                // this END like any other. `end_is_final` retires it otherwise.
                a.capped_at_ns = Some(now_ns);
                ends.push(ev);
            }
        }

        // ENDs first: an unsent END is an over-claim, an unsent START is a box that appears one
        // tick later.
        ends.extend(opens);
        ends
    }

    /// Publishes one tick's endpoints when one is due, returning `(published, deferred)`.
    ///
    /// Never blocks and never fails the caller: the publisher's own drop policy loses records for a
    /// slow consumer (ADR-0004 §7), never the detection thread that called this.
    pub fn tick(
        &mut self,
        now_ns: i64,
        extents: &[LiveExtent],
        extents_complete: bool,
        emitter_of: impl Fn(TrackId) -> Option<EmitterId>,
    ) -> (usize, usize) {
        if !self.due(now_ns) {
            return (0, 0);
        }
        self.last_ns = Some(now_ns);
        for ev in self.plan(now_ns, extents, extents_complete, emitter_of) {
            self.deferred.push_back(ev);
        }
        // Oldest first past the ceiling. Reached only above 128 endpoints a second, where the poll
        // is the honest backstop; counted so it is never silent.
        let mut lost = 0;
        while self.deferred.len() > MAX_DEFERRED_EVENTS {
            self.deferred.pop_front();
            lost += 1;
        }
        let Some(publisher) = self.publisher.as_mut() else {
            self.deferred.clear();
            return (0, 0);
        };
        let mut published = 0;
        while published < MAX_EVENTS_PER_TICK {
            let Some(ev) = self.deferred.pop_front() else {
                break;
            };
            if publisher.publish_message(&ev.record()).is_ok() {
                published += 1;
            }
        }
        (published, self.deferred.len() + lost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as Map;

    const S: i64 = 1_000_000_000;

    /// An extent whose silence the receiver watched through: `observed == wall`.
    fn watched(track: TrackId, start_ns: i64, end_ns: i64, silence_ns: i64) -> LiveExtent {
        LiveExtent {
            track,
            t_start_ns: start_ns,
            t_end_ns: end_ns,
            observed_silence_ns: silence_ns,
            wall_silence_ns: silence_ns,
        }
    }

    /// Stands in for `Inventory::emitter_of_track`: only tracks the inventory gave a row resolve.
    fn rows(pairs: &[(TrackId, EmitterId)]) -> impl Fn(TrackId) -> Option<EmitterId> + use<> {
        let map: Map<TrackId, EmitterId> = pairs.iter().copied().collect();
        move |t| map.get(&t).copied()
    }

    fn stream() -> PresenceStream {
        PresenceStream::new(None).unwrap()
    }

    /// The capture instant a tick carrying these extents runs at: the newest measured end plus the
    /// silence observed since it, which is what the frame clock reads when the batch is flushed.
    fn now_of(extents: &[LiveExtent]) -> i64 {
        extents
            .iter()
            .map(|e| e.t_end_ns.saturating_add(e.wall_silence_ns))
            .max()
            .unwrap_or(0)
    }

    /// `plan` at the instant its extents describe.
    fn plan(
        s: &mut PresenceStream,
        extents: &[LiveExtent],
        complete: bool,
        who: impl Fn(TrackId) -> Option<EmitterId>,
    ) -> Vec<PresenceEvent> {
        s.plan(now_of(extents), extents, complete, who)
    }

    /// The wire shape of the endpoint kinds: `t_ns` is the instant the record is *about*, the
    /// metadata is the inventory row's own `last_interval` in seconds (T-354), and neither carries
    /// geometry (T-362).
    #[test]
    fn a_record_states_its_endpoint_in_both_units_and_no_frequency() {
        let emitter = EmitterId::new();
        let (t0, t1) = (1_500_000_000 * S, 1_500_000_012 * S + S / 2);
        let start = PresenceEvent {
            kind: PresenceEventKind::Start,
            emitter,
            t_start_ns: t0,
            t_end_ns: t1,
            revoked_ns: 0,
        };
        let rec = start.record();
        assert_eq!(
            rec.t.as_unix_nanos(),
            t0,
            "an opening record is about its start"
        );
        assert_eq!(rec.frame_model.as_deref(), Some(PRESENCE_START_KIND));
        assert_eq!(rec.metadata["last_interval"]["open"], json!(true));
        assert_eq!(rec.emitter_id, Some(emitter));
        assert_eq!(rec.content_class, ContentClass::Unrestricted);
        assert!(rec.content.is_none(), "timing only, never content");

        let end = PresenceEvent {
            kind: PresenceEventKind::End,
            ..start
        };
        let rec = end.record();
        assert_eq!(
            rec.t.as_unix_nanos(),
            t1,
            "a closing record is about its measured end"
        );
        assert_eq!(rec.frame_model.as_deref(), Some(PRESENCE_END_KIND));
        let iv = &rec.metadata["last_interval"];
        assert_eq!(iv["t_start_s"].as_f64().unwrap(), 1_500_000_000.0);
        assert_eq!(iv["t_end_s"].as_f64().unwrap(), 1_500_000_012.5);
        assert_eq!(iv["open"], json!(false));
        assert_eq!(iv["revoked_s"].as_f64().unwrap(), 0.0);
        let text = rec.metadata.to_string();
        for banned in ["f_lo", "f_hi", "f_center", "bandwidth"] {
            assert!(
                !text.contains(banned),
                "an endpoint is time, not geometry: {banned}"
            );
        }

        // T-413: a REVOKE withdraws that END. It is about the measured edge the interval has
        // reached, states the same `t_start_s` (one interval, not a second box), and carries the
        // silence the receiver measured inside it.
        let revoke = PresenceEvent {
            kind: PresenceEventKind::Revoke,
            revoked_ns: 3 * S / 2,
            ..start
        };
        let rec = revoke.record();
        assert_eq!(
            rec.t.as_unix_nanos(),
            t1,
            "about the re-opened measured edge"
        );
        assert_eq!(rec.frame_model.as_deref(), Some(PRESENCE_REVOKE_KIND));
        let iv = &rec.metadata["last_interval"];
        assert_eq!(iv["open"], json!(true), "the interval is open again");
        assert_eq!(iv["t_start_s"].as_f64().unwrap(), 1_500_000_000.0);
        assert_eq!(iv["revoked_s"].as_f64().unwrap(), 1.5);
    }

    /// **The property contract B is for.** A signal that keeps transmitting produces exactly one
    /// record over its whole life — no per-tick bump — because the box is already drawn to the live
    /// edge and a continuing interval is not news.
    #[test]
    fn a_continuing_interval_emits_nothing_after_its_start() {
        let (track, emitter) = (TrackId::new(), EmitterId::new());
        let mut s = stream();
        let who = rows(&[(track, emitter)]);
        let first = plan(&mut s, &[watched(track, 0, S, 0)], true, &who);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].kind, PresenceEventKind::Start);
        for k in 2..40 {
            let later = plan(&mut s, &[watched(track, 0, k * S, 0)], true, &who);
            assert!(later.is_empty(), "tick {k} said something: {later:?}");
        }
    }

    /// The end detector, in both clocks. A watched silence closes at the 1 s floor; the same
    /// silence on a receiver that looked away does not, because the absence was never observed.
    #[test]
    fn an_interval_closes_on_watched_silence_and_defers_on_unwatched_silence() {
        let (track, emitter) = (TrackId::new(), EmitterId::new());
        let who = rows(&[(track, emitter)]);

        // Watched: nothing at the gap, closed just past it.
        let mut s = stream();
        plan(&mut s, &[watched(track, 0, 10 * S, 0)], true, &who);
        assert!(plan(&mut s, &[watched(track, 0, 10 * S, S)], true, &who).is_empty());
        let end = plan(&mut s, &[watched(track, 0, 10 * S, S + S / 4)], true, &who);
        assert_eq!(end.len(), 1);
        assert_eq!(end[0].kind, PresenceEventKind::End);
        assert_eq!(
            end[0].t_end_ns,
            10 * S,
            "caps at the MEASURED end, not at now"
        );

        // Looked away: five seconds of wall silence, only 50 ms of it observed. Not evidence.
        let mut s = stream();
        plan(&mut s, &[watched(track, 0, 10 * S, 0)], true, &who);
        let swept = LiveExtent {
            observed_silence_ns: S / 20,
            wall_silence_ns: 5 * S,
            ..watched(track, 0, 10 * S, 0)
        };
        assert!(
            plan(&mut s, &[swept], true, &who).is_empty(),
            "no absence was observed"
        );
        assert!(
            interval_closed(&LiveExtent {
                observed_silence_ns: 61 * S,
                wall_silence_ns: 600 * S,
                ..swept
            }),
            "past the conservative gap even an unwatched silence closes"
        );
    }

    /// **A silence inside the gap never ends the interval at all** — the user's "null the end and
    /// keep the single interval open", satisfied before an end exists to null.
    #[test]
    fn a_silence_inside_the_gap_never_ends_the_interval() {
        let (track, emitter) = (TrackId::new(), EmitterId::new());
        let mut s = stream();
        let who = rows(&[(track, emitter)]);

        let opened = plan(&mut s, &[watched(track, 0, 5 * S, 0)], true, &who);
        assert_eq!(opened.len(), 1);
        assert!(plan(&mut s, &[watched(track, 0, 5 * S, S / 2)], true, &who).is_empty());
        assert!(plan(&mut s, &[watched(track, 0, 6 * S, 0)], true, &who).is_empty());
    }

    /// **T-413: the END is provisional, and a resumption inside the window revokes it.** One
    /// interval, one row, the same `t_start_ns` — never a second box.
    #[test]
    fn a_resumption_inside_the_revocation_window_revokes_the_end() {
        let (track, emitter) = (TrackId::new(), EmitterId::new());
        let mut s = stream();
        let who = rows(&[(track, emitter)]);

        plan(&mut s, &[watched(track, 0, 5 * S, 0)], true, &who);
        // A watched silence past the 1 s gap caps it at the measured end.
        let end = plan(&mut s, &[watched(track, 0, 5 * S, S + S / 4)], true, &who);
        assert_eq!(end.len(), 1);
        assert_eq!(end[0].kind, PresenceEventKind::End);
        assert_eq!(end[0].t_end_ns, 5 * S);

        // The same track comes back 1.6 s past the measured end — inside `2 × gap`. The END is
        // withdrawn and the interval is the SAME interval, still starting at 0.
        let back = plan(
            &mut s,
            &[watched(track, 0, 5 * S + 8 * S / 5, 0)],
            true,
            &who,
        );
        assert_eq!(back.len(), 1, "expected a revocation: {back:?}");
        assert_eq!(back[0].kind, PresenceEventKind::Revoke);
        assert_eq!(back[0].t_start_ns, 0, "one interval, not a new one");
        assert_eq!(back[0].t_end_ns, 5 * S + 8 * S / 5);
        assert!(
            back[0].revoked_ns > 0,
            "it states the measured silence it rejoined: {back:?}"
        );
        assert!(
            back[0].revoked_ns <= 8 * S / 5,
            "and never more than the gap really was"
        );

        // And it is open again: continuing after a revocation is news to nobody.
        assert!(plan(&mut s, &[watched(track, 0, 10 * S, 0)], true, &who).is_empty());
    }

    /// **The far side of the boundary, asserted rather than assumed.** A resumption past
    /// `2 × gap` leaves the END standing; the interval is not rejoined, and this stream says
    /// nothing further about the track (the poll serves the new interval — ADR-0019 §6.1).
    #[test]
    fn a_resumption_outside_the_revocation_window_leaves_the_end_standing() {
        let (track, emitter) = (TrackId::new(), EmitterId::new());
        let mut s = stream();
        let who = rows(&[(track, emitter)]);

        plan(&mut s, &[watched(track, 0, 5 * S, 0)], true, &who);
        let end = plan(&mut s, &[watched(track, 0, 5 * S, S + S / 4)], true, &who);
        assert_eq!(end[0].kind, PresenceEventKind::End);

        // Still silent at 2.5 s: past the window, so the end becomes final. (Silence alone
        // publishes nothing — the END has already been sent.)
        assert!(plan(&mut s, &[watched(track, 0, 5 * S, 5 * S / 2)], true, &who).is_empty());
        // The track returns. Its only available start is `t_first` at 0, on the far side of the
        // capped silence, so there is no honest opening record and none is published.
        let back = plan(&mut s, &[watched(track, 0, 20 * S, 0)], true, &who);
        assert!(back.is_empty(), "the end stands: {back:?}");
    }

    /// **The boundary itself**, on both sides of one nanosecond, using a resumption whose own start
    /// is known (a new track on the same emitter) so the gap is exact rather than tick-quantised.
    #[test]
    fn the_revocation_window_is_two_idle_gaps_from_the_measured_end() {
        let gap = (MIN_IDLE_GAP_S * NS_PER_S) as i64;
        let run = |resume_at: i64| {
            let emitter = EmitterId::new();
            let (first, second) = (TrackId::new(), TrackId::new());
            let mut s = stream();
            let who = rows(&[(first, emitter), (second, emitter)]);
            plan(&mut s, &[watched(first, 0, 5 * S, 0)], true, &who);
            let end = plan(
                &mut s,
                &[watched(first, 0, 5 * S, gap + gap / 4)],
                true,
                &who,
            );
            assert_eq!(end[0].kind, PresenceEventKind::End);
            plan(
                &mut s,
                &[watched(second, resume_at, resume_at + S / 10, 0)],
                true,
                &who,
            )
        };

        let inside = run(5 * S + 2 * gap);
        assert_eq!(inside.len(), 1);
        assert_eq!(inside[0].kind, PresenceEventKind::Revoke, "at the edge");
        assert_eq!(inside[0].t_start_ns, 0, "the interval the END capped");
        assert_eq!(
            inside[0].revoked_ns,
            2 * gap,
            "the gap is known exactly here, so it is stated exactly"
        );

        let outside = run(5 * S + 2 * gap + 1);
        assert_eq!(outside.len(), 1, "{outside:?}");
        assert_eq!(
            outside[0].kind,
            PresenceEventKind::Reopen,
            "one nanosecond past the window is a genuinely new interval"
        );
        assert_eq!(outside[0].t_start_ns, 5 * S + 2 * gap + 1);
    }

    /// A returning signal on the same emitter, far past the window, reopens it — and the two
    /// intervals are two records and two boxes, never one box stretched across the silence.
    #[test]
    fn a_returning_signal_reopens_the_same_emitter_and_never_spans_the_silence() {
        let emitter = EmitterId::new();
        let (first, second) = (TrackId::new(), TrackId::new());
        let mut s = stream();
        let who = rows(&[(first, emitter), (second, emitter)]);

        plan(&mut s, &[watched(first, 0, 5 * S, 0)], true, &who);
        let end = plan(&mut s, &[watched(first, 0, 5 * S, 2 * S)], true, &who);
        assert_eq!(end[0].kind, PresenceEventKind::End);

        // A new track on the same emitter, starting 25 s after the silence.
        let out = plan(&mut s, &[watched(second, 30 * S, 31 * S, 0)], true, &who);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].kind,
            PresenceEventKind::Reopen,
            "same emitter, new interval"
        );
        assert_eq!(out[0].t_start_ns, 30 * S, "the new interval's own start");
        assert_eq!(
            end[0].t_end_ns,
            5 * S,
            "and the old one still ends where it ended"
        );

        // A fresh emitter with no history starts rather than reopens. (Its own stream, so the
        // emitter above staying open is not read as vanished and closed in the same breath.)
        let (t3, other) = (TrackId::new(), EmitterId::new());
        let out = plan(
            &mut stream(),
            &[watched(t3, 40 * S, 41 * S, 0)],
            true,
            rows(&[(t3, other)]),
        );
        assert_eq!(out[0].kind, PresenceEventKind::Start);
    }

    /// The belt-and-braces END: a track that vanishes (closed, or its row unbound) still caps its
    /// box — but only when the extent list is known complete, so a truncated batch can never
    /// fabricate an end for a signal still on the air. T-413: that END is provisional like any
    /// other, so the entry is held for one revocation window and only then forgotten.
    #[test]
    fn a_vanished_track_is_ended_but_only_from_a_complete_list() {
        let (track, emitter) = (TrackId::new(), EmitterId::new());
        let who = rows(&[(track, emitter)]);

        let mut s = stream();
        s.plan(5 * S, &[watched(track, 0, 5 * S, 0)], true, &who);
        assert!(
            s.plan(5 * S, &[], false, &who).is_empty(),
            "a truncated list ends nothing"
        );
        let end = s.plan(5 * S, &[], true, &who);
        assert_eq!(end.len(), 1);
        assert_eq!(end[0].kind, PresenceEventKind::End);
        assert_eq!(end[0].t_end_ns, 5 * S, "the last end it was announced with");
        // Ending it twice would be a duplicate cap; the entry is waiting out its window.
        assert!(s.plan(5 * S + S / 2, &[], true, &who).is_empty());
        assert!(s.has_state(), "still revocable");
        // Past `2 × gap` (plus the tick slack) it is final and nothing is left.
        assert!(s.plan(5 * S + 4 * S, &[], true, &who).is_empty());
        assert!(!s.has_state(), "and nothing is left open");
    }

    /// An extent for a track the inventory has given no row is not addressed to anything.
    #[test]
    fn a_track_with_no_inventory_row_yields_no_event() {
        let (with_row, without) = (TrackId::new(), TrackId::new());
        let mut s = stream();
        let out = plan(
            &mut s,
            &[watched(with_row, 0, S, 0), watched(without, 0, S, 0)],
            true,
            rows(&[(with_row, EmitterId::new())]),
        );
        assert_eq!(out.len(), 1);
    }

    /// The rate ceiling, and the truncation policy contract B forced: ENDs are published first,
    /// and what does not fit is **carried**, not dropped.
    #[test]
    fn a_tick_publishes_ends_first_and_carries_the_rest() {
        let mut s = stream();
        let n = MAX_EVENTS_PER_TICK + 10;
        let (mut pairs, mut extents) = (Vec::new(), Vec::new());
        for i in 0..n {
            let (t, e) = (TrackId::new(), EmitterId::new());
            pairs.push((t, e));
            extents.push(watched(t, 0, (i as i64 + 1) * S, 0));
        }
        let who = rows(&pairs);
        // Everything opens…
        let opens = plan(&mut s, &extents, true, &who);
        assert_eq!(opens.len(), n);
        // …then one of them stops while the rest keep going. Its END leads the next plan.
        let mut next = extents.clone();
        next[n - 1].observed_silence_ns = 2 * S;
        next[n - 1].wall_silence_ns = 2 * S;
        let mut s2 = stream();
        plan(&mut s2, &extents, true, &who);
        let out = plan(&mut s2, &next, true, &who);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, PresenceEventKind::End);

        // The cap is on records per tick; the remainder is deferred, and counted.
        let mut s3 = PresenceStream::new(None).unwrap();
        plan(&mut s3, &extents, true, &who)
            .into_iter()
            .for_each(|e| s3.deferred.push_back(e));
        assert_eq!(s3.deferred.len(), n);
    }

    /// The tick gate, which is the other half of the rate bound.
    #[test]
    fn ticks_are_no_closer_together_than_the_push_period() {
        let mut s = stream();
        assert!(s.due(0));
        s.tick(0, &[], true, |_| None);
        assert!(!s.due(PRESENCE_PUSH_NS - 1));
        assert!(s.due(PRESENCE_PUSH_NS));
    }
}
