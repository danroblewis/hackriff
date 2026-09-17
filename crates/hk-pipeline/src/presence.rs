//! The live presence-extension push (T-388, ADR-0004 §15 / `docs/stream-contract.md` §15): the
//! `presence` stream, one `messages` record per open track per tick saying **how far that emitter's
//! presence has now been observed**.
//!
//! # The problem it exists for
//!
//! A live signal's box grew in steps of ten seconds or so. Detection was never the bottleneck — the
//! tracker advances an open track's end (`Tracker::apply`, `t_last_end`) on **every STFT frame**.
//! What was slow was everything between that and the screen:
//!
//! | link | cadence before T-388 |
//! |---|---|
//! | tracker advances `t_last_end` | per frame |
//! | detect reader flushes a `WriteBatch` | `flush_interval_s`, 0.5 s |
//! | **open track offered to the inventory** (`LIVE_OFFER_NS`) | **5 s** |
//! | writer thread stores it | ≤ 200 ms |
//! | UI `GET /api/inventory` poll | **5 s** |
//!
//! Two lazy links in series, so a box top sat between 5 s and 10 s behind the live edge. Shortening
//! only the poll would have fixed half of it and left the other half; shortening only the offer
//! would have fixed the other half and left the poll. This module removes both at once by carrying
//! the extension on its own path — the offer and the poll are untouched and still do their jobs
//! (creating the row, arbitrating it, merging it, confirming it, serving a *window*).
//!
//! # What it may say, and what it may not
//!
//! **A box may only extend as far as presence has actually been observed.** The time this stream
//! publishes is [`LiveExtent::t_end_ns`] — the end of the last burst the detector *measured* — never
//! a clock read and never an assumption that a signal heard a moment ago is still transmitting. The
//! consequence is the property that matters: **an emission that stops stops extending**, within one
//! tick, because the tracker stops advancing `t_last_end` and this stream can only repeat the end it
//! was given. It is the same rule as `Coverage::of` (T-368) refusing to spell "never looked" as
//! "looked and it was quiet", and as T-385's absent row meaning out-of-window rather than deleted.
//!
//! Extrapolating on the client — drawing the box to the live edge because a signal *was* there a
//! second ago — would be faster still and would be a claim about air nobody measured. That is why
//! the fix is a push.
//!
//! # Shape
//!
//! `messages` kind, schema [`PRESENCE_MESSAGE_SCHEMA`], `content_class` unrestricted. One record per
//! extension:
//!
//! ```json
//! {"type":"message","seq":7,"t_ns":1757774400123456789,"emitter_id":"0199…",
//!  "content_class":"unrestricted","gated":false,"frame_model":"presence-extension",
//!  "metadata":{"kind":"presence-extension",
//!              "last_interval":{"t_start_s":1757774390.1,"t_end_s":1757774400.12,"open":true}}}
//! ```
//!
//! The envelope's `t_ns` is the extension's own end instant (T-354: the stream contract carries
//! `t_ns`, integer Unix nanoseconds, never a bare `t`). `metadata.last_interval` is deliberately the
//! **same object** `GET /api/inventory` serves as `presence.last_interval` — same three field names,
//! same `_s` seconds unit (`docs/api.md`) — so a client assigns it rather than converting it, and
//! the fast surface cannot invent a shape the slow one would disagree with.
//!
//! **No frequency.** A presence extension is new *time*, not new geometry (T-362: a `TimeBox` is a
//! band fraction plus two absolute capture times). The box's edges came from the row and are not
//! restated, so this path can never move a box sideways.
//!
//! # Rate, and what bounds it
//!
//! Emission is gated twice: at most one tick per [`PRESENCE_PUSH_NS`] of stream time, and at most
//! [`MAX_EXTENSIONS_PER_TICK`] records in a tick. That is a hard ceiling of
//! `MAX_EXTENSIONS_PER_TICK / PRESENCE_PUSH_NS` records per second — 128/s as configured — however
//! busy the band is, because the cap is on records and not on tracks. Beyond it, the tracks left out
//! of a tick simply are not extended in it (counted as `presence_extensions_truncated`); their boxes
//! grow on the 5 s inventory poll as they did before. A slow consumer is dropped by the publisher's
//! own policy (ADR-0004 backpressure), never the survey.

use hk_detect::LiveExtent;
use hk_model::{ContentClass, EmitterId, Timestamp, TrackId};
use hk_stream::{MessageRecord, Publisher, PublisherConfig, StreamHeader, StreamKind};
use serde_json::json;

use crate::config::StreamSink;

/// Stream id of the presence-extension stream (`/ws/presence`).
pub const PRESENCE_STREAM_ID: &str = "presence";
/// Message schema of the presence-extension stream.
pub const PRESENCE_MESSAGE_SCHEMA: &str = "hackriff.presence/1";
/// `frame_model` every record carries, and the `metadata.kind` beside it.
pub const PRESENCE_EXTENSION_KIND: &str = "presence-extension";

/// Shortest gap between ticks, stream ns. 250 ms: well inside the ~1 s the box top must track the
/// live edge to, and four times the 0.5 s detect flush that feeds it, so the flush — not this — is
/// what actually paces the stream.
pub const PRESENCE_PUSH_NS: i64 = 250_000_000;

/// Records one tick may publish. The rate ceiling (see the module docs); not a claim about how many
/// emitters are on the air.
pub const MAX_EXTENSIONS_PER_TICK: usize = 32;

/// One extension as the wire sees it: an emitter, and how far its presence has been observed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PresenceExtension {
    /// The emitter whose row the client is holding.
    pub emitter: EmitterId,
    /// Start of the observed span, Unix ns.
    pub t_start_ns: i64,
    /// **End of the observed span**, Unix ns — the last measured burst end, never `now`.
    pub t_end_ns: i64,
}

/// Unix seconds of a nanosecond instant, the `_s` unit `docs/api.md` and the inventory row use.
fn secs(ns: i64) -> f64 {
    ns as f64 * 1e-9
}

impl PresenceExtension {
    /// The record this extension publishes as. Pure, so the wire shape is unit-tested without a
    /// publisher (see the tests at the foot of this module).
    pub fn record(&self) -> MessageRecord {
        MessageRecord {
            t: Timestamp::from_unix_nanos(self.t_end_ns),
            emitter_id: Some(self.emitter),
            provenance_ref: None,
            // Timing of exactly the class `/api/inventory`'s own `presence` object carries
            // unconditionally (`hk_api::presence`: serving it uniformly is what keeps its presence
            // from signalling that a row's identity was withheld). No identity, no content.
            content_class: ContentClass::Unrestricted,
            decode_id: None,
            annotation_id: None,
            decoder: None,
            frame_model: Some(PRESENCE_EXTENSION_KIND.into()),
            crc_status: None,
            identity: None,
            metadata: json!({
                "kind": PRESENCE_EXTENSION_KIND,
                // The `presence.last_interval` object verbatim (docs/api.md), so the client assigns
                // it instead of rebuilding it. `open` is stated here, never inferred there: this
                // record exists because the tracker still holds the track open.
                "last_interval": {
                    "t_start_s": secs(self.t_start_ns),
                    "t_end_s": secs(self.t_end_ns),
                    "open": true,
                },
            }),
            content: None,
        }
    }
}

/// The run's presence-extension publisher: the tick gate and the stream itself.
///
/// It holds no track→emitter map of its own. The join is asked of the inventory
/// ([`crate::Inventory::emitter_of_track`]) at each tick, which is the one place that knows it and
/// the one place that stays right through a merge or a link — a second copy here would be a second
/// opinion about which box a signal is.
pub struct PresenceStream {
    publisher: Option<Publisher>,
    /// Stream time of the last tick.
    last_ns: Option<i64>,
}

impl PresenceStream {
    /// Offers the `presence` stream through `sink`, when there is one.
    pub(crate) fn new(sink: Option<&StreamSink>) -> anyhow::Result<Self> {
        let publisher = match sink {
            Some(sink) => {
                let mut header = StreamHeader::new(
                    PRESENCE_STREAM_ID,
                    StreamKind::Messages,
                    // Timing metadata (how far an emitter has been heard), never RF content.
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
        })
    }

    /// Whether `now_ns` is far enough past the last tick to publish again.
    pub fn due(&self, now_ns: i64) -> bool {
        self.last_ns
            .is_none_or(|t| now_ns.saturating_sub(t) >= PRESENCE_PUSH_NS)
    }

    /// The extensions `extents` resolves to through `emitter_of`, newest end first and capped at
    /// [`MAX_EXTENSIONS_PER_TICK`]; the second value is how many were left out by the cap. An
    /// extent whose track has no emitter yields nothing: there is no row on screen to extend, and
    /// inventing an id would address a box that does not exist.
    ///
    /// Newest-end-first is the only ordering that matters under the cap: the tracks closest to the
    /// live edge are the ones a viewer is watching grow.
    pub fn resolve(
        extents: &[LiveExtent],
        emitter_of: impl Fn(TrackId) -> Option<EmitterId>,
    ) -> (Vec<PresenceExtension>, usize) {
        let mut out: Vec<PresenceExtension> = extents
            .iter()
            .filter_map(|e| {
                emitter_of(e.track).map(|emitter| PresenceExtension {
                    emitter,
                    t_start_ns: e.t_start_ns,
                    t_end_ns: e.t_end_ns,
                })
            })
            .collect();
        out.sort_unstable_by(|a, b| b.t_end_ns.cmp(&a.t_end_ns));
        let truncated = out.len().saturating_sub(MAX_EXTENSIONS_PER_TICK);
        out.truncate(MAX_EXTENSIONS_PER_TICK);
        (out, truncated)
    }

    /// Publishes one tick's extensions when one is due, returning `(published, truncated)`.
    ///
    /// Never blocks and never fails the caller: the publisher's own drop policy loses records for a
    /// slow consumer (ADR-0004 §7), never the detection thread that called this.
    pub fn tick(
        &mut self,
        now_ns: i64,
        extents: &[LiveExtent],
        emitter_of: impl Fn(TrackId) -> Option<EmitterId>,
    ) -> (usize, usize) {
        if !self.due(now_ns) {
            return (0, 0);
        }
        self.last_ns = Some(now_ns);
        let (list, truncated) = Self::resolve(extents, emitter_of);
        let Some(publisher) = self.publisher.as_mut() else {
            return (0, 0);
        };
        let mut published = 0;
        for ext in &list {
            if publisher.publish_message(&ext.record()).is_ok() {
                published += 1;
            }
        }
        (published, truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn extent(track: TrackId, start_ns: i64, end_ns: i64) -> LiveExtent {
        LiveExtent {
            track,
            t_start_ns: start_ns,
            t_end_ns: end_ns,
        }
    }

    /// Stands in for `Inventory::emitter_of_track`: only tracks the inventory gave a row resolve.
    fn rows(pairs: &[(TrackId, EmitterId)]) -> impl Fn(TrackId) -> Option<EmitterId> + use<> {
        let map: HashMap<TrackId, EmitterId> = pairs.iter().copied().collect();
        move |t| map.get(&t).copied()
    }

    /// The wire shape is the inventory row's own `presence.last_interval`, and the envelope time is
    /// the *observed* end — T-354's `t_ns`, in nanoseconds, matching the metadata's `_s` seconds.
    #[test]
    fn a_record_states_the_observed_end_in_both_units_and_no_frequency() {
        let ext = PresenceExtension {
            emitter: EmitterId::new(),
            t_start_ns: 1_500_000_000_000_000_000,
            t_end_ns: 1_500_000_012_500_000_000,
        };
        let rec = ext.record();
        assert_eq!(rec.t.as_unix_nanos(), ext.t_end_ns, "t_ns is the end");
        assert_eq!(rec.emitter_id, Some(ext.emitter));
        assert_eq!(rec.content_class, ContentClass::Unrestricted);
        assert!(rec.content.is_none(), "timing only, never content");
        let iv = &rec.metadata["last_interval"];
        assert_eq!(iv["t_start_s"].as_f64().unwrap(), 1_500_000_000.0);
        assert_eq!(iv["t_end_s"].as_f64().unwrap(), 1_500_000_012.5);
        assert_eq!(iv["open"], json!(true));
        let text = rec.metadata.to_string();
        for banned in ["f_lo", "f_hi", "f_center", "bandwidth"] {
            assert!(
                !text.contains(banned),
                "an extension is new time, not new geometry: {banned}"
            );
        }
    }

    /// An extent for a track the inventory has given no row is not addressed to anything, so
    /// nothing is published for it. That lookup is the only gate on what the wider extent predicate
    /// (`Tracker::live_extents_into`) hands over.
    #[test]
    fn a_track_with_no_inventory_row_yields_no_extension() {
        let (with_row, without) = (TrackId::new(), TrackId::new());
        let emitter_of = rows(&[(with_row, EmitterId::new())]);
        let (list, truncated) = PresenceStream::resolve(
            &[extent(with_row, 0, 10), extent(without, 0, 10)],
            emitter_of,
        );
        assert_eq!(list.len(), 1);
        assert_eq!(truncated, 0);
    }

    /// The hard rate ceiling: a tick publishes at most `MAX_EXTENSIONS_PER_TICK` records however
    /// many tracks are live, keeping the newest ends, and says how many it left out.
    #[test]
    fn a_tick_is_capped_and_keeps_the_ends_closest_to_the_live_edge() {
        let n = MAX_EXTENSIONS_PER_TICK + 10;
        let mut pairs = Vec::new();
        let mut extents = Vec::new();
        for i in 0..n {
            let t = TrackId::new();
            pairs.push((t, EmitterId::new()));
            extents.push(extent(t, 0, i as i64));
        }
        let (list, truncated) = PresenceStream::resolve(&extents, rows(&pairs));
        assert_eq!(list.len(), MAX_EXTENSIONS_PER_TICK);
        assert_eq!(truncated, 10);
        assert_eq!(list[0].t_end_ns, (n - 1) as i64, "newest end first");
    }

    /// The tick gate, which is the other half of the rate bound.
    #[test]
    fn ticks_are_no_closer_together_than_the_push_period() {
        let mut s = PresenceStream::new(None).unwrap();
        assert!(s.due(0));
        s.tick(0, &[], |_| None);
        assert!(!s.due(PRESENCE_PUSH_NS - 1));
        assert!(s.due(PRESENCE_PUSH_NS));
    }

    /// **The control that matters.** A signal that stops stops extending: the tracker stops
    /// advancing `t_last_end`, so every later tick can only repeat the end that was observed. There
    /// is nowhere in this path for "now" to enter.
    #[test]
    fn a_stopped_emission_stops_extending() {
        let track = TrackId::new();
        let emitter = EmitterId::new();
        let last_observed_ns = 4_000_000_000;
        let (list, _) = PresenceStream::resolve(
            &[extent(track, 0, last_observed_ns)],
            rows(&[(track, emitter)]),
        );
        assert_eq!(list[0].t_end_ns, last_observed_ns);
        // Ten seconds of wall time later, with the tracker holding the same end (it saw nothing
        // since), the extension is the same instant — not the clock.
        let (later, _) = PresenceStream::resolve(
            &[extent(track, 0, last_observed_ns)],
            rows(&[(track, emitter)]),
        );
        assert_eq!(later[0].t_end_ns, last_observed_ns);
        // And once the track closes, the inventory forgets it and nothing is published at all.
        let (gone, _) = PresenceStream::resolve(&[extent(track, 0, last_observed_ns)], rows(&[]));
        assert!(gone.is_empty());
    }
}
