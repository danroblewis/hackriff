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
/// every endpoint on the stream.
pub const PRESENCE_MESSAGE_SCHEMA: &str = "hackriff.presence/2";

/// `metadata.kind` (and `frame_model`) of a record opening an emitter's **first** interval.
pub const PRESENCE_START_KIND: &str = "presence-start";
/// …opening a **later** interval on an emitter that has been on the air before.
pub const PRESENCE_REOPEN_KIND: &str = "presence-reopen";
/// …closing an interval, at its **measured** end.
pub const PRESENCE_END_KIND: &str = "presence-end";

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
    let watched = e.observed_silence_ns >= e.wall_silence_ns.saturating_sub(COVERAGE_SLACK_NS);
    let gap_s = if watched {
        MIN_IDLE_GAP_S
    } else {
        MAX_IDLE_GAP_S
    };
    e.observed_silence_ns > (gap_s * NS_PER_S) as i64
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
    /// An interval closed, at its measured end.
    End,
}

impl PresenceEventKind {
    /// The wire `kind` / `frame_model`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => PRESENCE_START_KIND,
            Self::Reopen => PRESENCE_REOPEN_KIND,
            Self::End => PRESENCE_END_KIND,
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
}

/// Unix seconds of a nanosecond instant, the `_s` unit `docs/api.md` and the inventory row use.
fn secs(ns: i64) -> f64 {
    ns as f64 * 1e-9
}

impl PresenceEvent {
    /// The record this event publishes as. Pure, so the wire shape is unit-tested without a
    /// publisher (see the tests at the foot of this module).
    pub fn record(&self) -> MessageRecord {
        // The instant the record is *about*: where a box's newest edge lands because of it.
        let t = if self.kind.open() {
            self.t_start_ns
        } else {
            self.t_end_ns
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
                },
            }),
            content: None,
        }
    }
}

/// An interval this stream has announced as open, and has not yet announced the end of.
#[derive(Clone, Copy, Debug)]
struct Announced {
    track: TrackId,
    t_start_ns: i64,
    t_end_ns: i64,
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
    /// Emitters whose interval this stream has opened and not yet closed.
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
        extents: &[LiveExtent],
        extents_complete: bool,
        emitter_of: impl Fn(TrackId) -> Option<EmitterId>,
    ) -> Vec<PresenceEvent> {
        let mut ends: Vec<PresenceEvent> = Vec::new();
        let mut opens: Vec<PresenceEvent> = Vec::new();
        let mut seen: Vec<EmitterId> = Vec::with_capacity(extents.len());

        for e in extents {
            // An extent for a track the inventory has given no row is not addressed to anything:
            // there is no box on screen, and inventing an id would address one that does not exist.
            let Some(emitter) = emitter_of(e.track) else {
                continue;
            };
            seen.push(emitter);
            let closed = interval_closed(e);
            match self.open.get(&emitter).copied() {
                // A different, later interval on the same emitter: the announced one ended where it
                // was last measured, and this one opens. Emitting both keeps the silence between
                // them drawn as silence — a REOPEN never stretches a box across it (ADR-0019 §6).
                Some(a) if e.t_start_ns > a.t_start_ns => {
                    ends.push(PresenceEvent {
                        kind: PresenceEventKind::End,
                        emitter,
                        t_start_ns: a.t_start_ns,
                        t_end_ns: a.t_end_ns,
                    });
                    self.open.remove(&emitter);
                    self.remember(emitter, e.track);
                    if !closed {
                        opens.push(PresenceEvent {
                            kind: PresenceEventKind::Reopen,
                            emitter,
                            t_start_ns: e.t_start_ns,
                            t_end_ns: e.t_end_ns,
                        });
                        self.open.insert(
                            emitter,
                            Announced {
                                track: e.track,
                                t_start_ns: e.t_start_ns,
                                t_end_ns: e.t_end_ns,
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
                        });
                        self.open.remove(&emitter);
                        self.remember(emitter, e.track);
                    } else if e.t_end_ns > a.t_end_ns {
                        self.open.insert(
                            emitter,
                            Announced {
                                track: e.track,
                                t_start_ns: a.t_start_ns,
                                t_end_ns: e.t_end_ns,
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
                    });
                    self.open.insert(
                        emitter,
                        Announced {
                            track: e.track,
                            t_start_ns: e.t_start_ns,
                            t_end_ns: e.t_end_ns,
                        },
                    );
                }
                None => {}
            }
        }

        // The belt-and-braces END: an announced emitter with no extent at all. Its track closed
        // (idle, capacity, end of stream), or its row was unbound. Either way the box must cap, and
        // this is what makes "there is no path that produces no END" true.
        if extents_complete {
            let gone: Vec<(EmitterId, TrackId)> = self
                .open
                .iter()
                .filter(|(e, _)| !seen.contains(e))
                .map(|(e, a)| (*e, a.track))
                .collect();
            for (emitter, track) in gone {
                let a = self.open.remove(&emitter).expect("key came from the map");
                self.remember(emitter, track);
                ends.push(PresenceEvent {
                    kind: PresenceEventKind::End,
                    emitter,
                    t_start_ns: a.t_start_ns,
                    t_end_ns: a.t_end_ns,
                });
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
        for ev in self.plan(extents, extents_complete, emitter_of) {
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

    /// The wire shape of both endpoint kinds: `t_ns` is the instant the record is *about*, the
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
        let text = rec.metadata.to_string();
        for banned in ["f_lo", "f_hi", "f_center", "bandwidth"] {
            assert!(
                !text.contains(banned),
                "an endpoint is time, not geometry: {banned}"
            );
        }
    }

    /// **The property contract B is for.** A signal that keeps transmitting produces exactly one
    /// record over its whole life — no per-tick bump — because the box is already drawn to the live
    /// edge and a continuing interval is not news.
    #[test]
    fn a_continuing_interval_emits_nothing_after_its_start() {
        let (track, emitter) = (TrackId::new(), EmitterId::new());
        let mut s = stream();
        let who = rows(&[(track, emitter)]);
        let first = s.plan(&[watched(track, 0, S, 0)], true, &who);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].kind, PresenceEventKind::Start);
        for k in 2..40 {
            let later = s.plan(&[watched(track, 0, k * S, 0)], true, &who);
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
        s.plan(&[watched(track, 0, 10 * S, 0)], true, &who);
        assert!(
            s.plan(&[watched(track, 0, 10 * S, S)], true, &who)
                .is_empty()
        );
        let end = s.plan(&[watched(track, 0, 10 * S, S + S / 4)], true, &who);
        assert_eq!(end.len(), 1);
        assert_eq!(end[0].kind, PresenceEventKind::End);
        assert_eq!(
            end[0].t_end_ns,
            10 * S,
            "caps at the MEASURED end, not at now"
        );

        // Looked away: five seconds of wall silence, only 50 ms of it observed. Not evidence.
        let mut s = stream();
        s.plan(&[watched(track, 0, 10 * S, 0)], true, &who);
        let swept = LiveExtent {
            observed_silence_ns: S / 20,
            wall_silence_ns: 5 * S,
            ..watched(track, 0, 10 * S, 0)
        };
        assert!(
            s.plan(&[swept], true, &who).is_empty(),
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

    /// **The end is provisional, and what may revoke it.** A silence shorter than the idle gap
    /// never produces an END at all, so a signal that blips off and returns inside it keeps one
    /// unbroken box and one interval — the user's "null the end and keep the single interval open",
    /// satisfied before an end exists to null (CLAUDE.md, ADR-0019 §6).
    ///
    /// Once an END *has* been published, the same track returning publishes nothing: its extent
    /// carries `t_first`, the track's first burst, and announcing that as a new interval's start
    /// would claim the silence the END was just drawn for. The poll serves the resumption with the
    /// start it actually has.
    #[test]
    fn a_silence_inside_the_gap_never_ends_the_interval_and_a_resumption_after_one_does_not_reclaim_it()
     {
        let (track, emitter) = (TrackId::new(), EmitterId::new());
        let mut s = stream();
        let who = rows(&[(track, emitter)]);

        // Open, then blip off for half the gap and come back: nothing at all is published, so the
        // box was never capped and the interval was never split.
        let opened = s.plan(&[watched(track, 0, 5 * S, 0)], true, &who);
        assert_eq!(opened.len(), 1);
        assert!(
            s.plan(&[watched(track, 0, 5 * S, S / 2)], true, &who)
                .is_empty()
        );
        assert!(
            s.plan(&[watched(track, 0, 6 * S, 0)], true, &who)
                .is_empty()
        );

        // Now a silence past the gap caps it…
        let end = s.plan(&[watched(track, 0, 6 * S, 2 * S)], true, &who);
        assert_eq!(end.len(), 1);
        assert_eq!(end[0].kind, PresenceEventKind::End);

        // …and the SAME track coming back says nothing, because the only start it could offer is
        // the track's first burst at 0 — which is on the far side of the silence just capped.
        let back = s.plan(&[watched(track, 0, 20 * S, 0)], true, &who);
        assert!(
            back.is_empty(),
            "a resumption whose only available start predates the capped silence must not be \
             published: {back:?}"
        );
    }

    /// A returning signal on the same emitter reopens it — and the two intervals are two records
    /// and two boxes, never one box stretched across the silence between them.
    #[test]
    fn a_returning_signal_reopens_the_same_emitter_and_never_spans_the_silence() {
        let emitter = EmitterId::new();
        let (first, second) = (TrackId::new(), TrackId::new());
        let mut s = stream();
        let who = rows(&[(first, emitter), (second, emitter)]);

        s.plan(&[watched(first, 0, 5 * S, 0)], true, &who);
        let end = s.plan(&[watched(first, 0, 5 * S, 2 * S)], true, &who);
        assert_eq!(end[0].kind, PresenceEventKind::End);

        // A new track on the same emitter, starting after the silence.
        let out = s.plan(&[watched(second, 30 * S, 31 * S, 0)], true, &who);
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
        let out = stream().plan(
            &[watched(t3, 40 * S, 41 * S, 0)],
            true,
            rows(&[(t3, other)]),
        );
        assert_eq!(out[0].kind, PresenceEventKind::Start);
    }

    /// The belt-and-braces END: a track that vanishes (closed, or its row unbound) still caps its
    /// box — but only when the extent list is known complete, so a truncated batch can never
    /// fabricate an end for a signal still on the air.
    #[test]
    fn a_vanished_track_is_ended_but_only_from_a_complete_list() {
        let (track, emitter) = (TrackId::new(), EmitterId::new());
        let who = rows(&[(track, emitter)]);

        let mut s = stream();
        s.plan(&[watched(track, 0, 5 * S, 0)], true, &who);
        assert!(
            s.plan(&[], false, &who).is_empty(),
            "a truncated list ends nothing"
        );
        let end = s.plan(&[], true, &who);
        assert_eq!(end.len(), 1);
        assert_eq!(end[0].kind, PresenceEventKind::End);
        assert_eq!(end[0].t_end_ns, 5 * S, "the last end it was announced with");
        assert!(!s.has_state(), "and nothing is left open");
    }

    /// An extent for a track the inventory has given no row is not addressed to anything.
    #[test]
    fn a_track_with_no_inventory_row_yields_no_event() {
        let (with_row, without) = (TrackId::new(), TrackId::new());
        let mut s = stream();
        let out = s.plan(
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
        let opens = s.plan(&extents, true, &who);
        assert_eq!(opens.len(), n);
        // …then one of them stops while the rest keep going. Its END leads the next plan.
        let mut next = extents.clone();
        next[n - 1].observed_silence_ns = 2 * S;
        next[n - 1].wall_silence_ns = 2 * S;
        let mut s2 = stream();
        s2.plan(&extents, true, &who);
        let out = s2.plan(&next, true, &who);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, PresenceEventKind::End);

        // The cap is on records per tick; the remainder is deferred, and counted.
        let mut s3 = PresenceStream::new(None).unwrap();
        s3.plan(&extents, true, &who)
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
