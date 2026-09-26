//! `GET /ws/tiles/changes` — **`coverage_changed` pushed on a retune** (T-1040), so a client
//! re-lays the coverage fog at once and re-asks exactly the tiles that moved, instead of waiting
//! for its survey timer and its refresh lane to come round.
//!
//! # What changes, and why a retune is the event
//!
//! A tile's coverage is a function of the tune record ([`crate::coverage`]): which band each front
//! end was on, when. Between retunes nothing about that function changes except that it reaches
//! further forward — which the live edge already carries (rows, `as_of_s`). What *does* change it is
//! a **move**: from the instant a front end leaves a band, that band's newest rows are `unobserved`
//! (the departed band's fog), and the band it arrives at turns observed. Both halves are the same
//! fact — one front end, one instant, two bands — so one event carries them:
//! `{f_lo, f_hi, t}` is the union of the departed and arrived bands and the instant of the move,
//! and every tile that intersects `[f_lo, f_hi] × [t, ∞)` is exactly the set a client holds a now
//! out-of-date copy of.
//!
//! # It reads the records that already exist
//!
//! Nothing new is recorded. The band each front end is on **now** is read from the same two tune
//! records the coverage map rasterises: the IQ ring's journal (a segment opens on every provenance
//! change) and, for a front end the ring holds nothing for — the ring refused, T-588/T-596 — the
//! observation log's dwell in flight. One source per front end, never a mix: the ring's segment is
//! the tuned window, a dwell's is the analysed one, and alternating between the two would report a
//! move that never happened. A provenance change that keeps the band (a gain step, a reseal) is not
//! a move and sends nothing. A front end that stops reporting sends nothing either: its fog just
//! stops advancing, which the rows already say.
//!
//! This route is the durable half of the user's 2026-09-25 "change events instead of re-asking"
//! (ticket C); its live-edge `tile_committed` half is superseded by the live-stream ring (LSR-1/2).

use std::collections::BTreeMap;
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

use hk_model::FreqRange;
use hk_model::attention::observation::ObservationRecord;
use serde_json::{Value, json};

use crate::http::ApiState;
use crate::query::{ApiError, Params};
use crate::rows::{FeedSlot, accept, peer_alive, refuse, send};

/// Change subscriptions open at once, per server. Past it the handshake is refused `503`.
pub const MAX_CHANGE_FEEDS: usize = 16;

/// How often a subscription reads the tune record. One read is the ring's newest segments and the
/// dwells in flight — both in memory — so this is a cost measured in microseconds, and it bounds
/// how late a `coverage_changed` can be after the record states the move.
pub const CHANGE_TICK: Duration = Duration::from_millis(100);

/// Newest ring segments read per tick. Several front ends each open a segment per provenance
/// change, so a handful of the newest is enough to hold every front end's current one.
const RING_NEWEST: usize = 64;

/// Two band edges closer than this are the same band, Hz.
const SAME_BAND_HZ: f64 = 1.0;

/// Query parameters this route accepts.
const ALLOWED: [&str; 1] = ["token"];

/// The band one front end is on now, per the tune record.
#[derive(Clone, Debug, PartialEq)]
pub struct Tuned {
    /// The front end (`device_id`, or `"unknown"` for a record that did not name one).
    pub device: String,
    /// Low edge of the band, Hz.
    pub f_lo_hz: f64,
    /// High edge of the band, Hz.
    pub f_hi_hz: f64,
    /// When the front end arrived on this band (the record's start), Unix ns.
    pub since_ns: i64,
    /// How far forward the record of it reaches, Unix ns.
    pub reach_ns: i64,
    /// Which record said so: `"iq-ring"` or `"open-dwell"`.
    pub source: &'static str,
}

impl Tuned {
    fn same_band(&self, o: &Tuned) -> bool {
        (self.f_lo_hz - o.f_lo_hz).abs() < SAME_BAND_HZ
            && (self.f_hi_hz - o.f_hi_hz).abs() < SAME_BAND_HZ
    }

    fn band_json(&self) -> Value {
        json!({ "f_lo": self.f_lo_hz, "f_hi": self.f_hi_hz })
    }

    fn json(&self) -> Value {
        json!({
            "device": self.device,
            "f_lo": self.f_lo_hz,
            "f_hi": self.f_hi_hz,
            "since_s": self.since_ns as f64 / 1e9,
            "as_of_s": self.reach_ns as f64 / 1e9,
            "source": self.source,
        })
    }
}

/// **The band every front end is on now** — the newest record per front end, one source each.
pub fn tuned_now(state: &ApiState) -> BTreeMap<String, Tuned> {
    let mut out: BTreeMap<String, Tuned> = BTreeMap::new();
    if let Some(ring) = state.iq_buffer.as_deref() {
        let status = ring.status(&crate::iqbuffer::IqBufferQuery {
            t0: None,
            t1: None,
            limit: RING_NEWEST,
        });
        for s in status
            .get("segments")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (Some(c), Some(r), Some(t0), Some(t1)) = (
                s.get("center_hz").and_then(Value::as_f64),
                s.get("sample_rate_hz")
                    .and_then(Value::as_f64)
                    .filter(|r| *r > 0.0),
                s.get("t0_ns").and_then(Value::as_i64),
                s.get("t1_ns").and_then(Value::as_i64),
            ) else {
                continue;
            };
            if t1 <= t0 {
                continue;
            }
            let device = s
                .get("device_id")
                .and_then(Value::as_str)
                .filter(|d| !d.is_empty())
                .unwrap_or("unknown")
                .to_owned();
            let t = Tuned {
                device: device.clone(),
                f_lo_hz: c - r / 2.0,
                f_hi_hz: c + r / 2.0,
                since_ns: t0,
                reach_ns: t1,
                source: "iq-ring",
            };
            match out.get(&device) {
                Some(have) if have.since_ns >= t.since_ns => {}
                _ => {
                    out.insert(device, t);
                }
            }
        }
    }
    // A front end the ring holds nothing for: its dwell in flight (T-596).
    if let Some(log) = state.observations.as_ref() {
        let mut dwells: BTreeMap<String, Tuned> = BTreeMap::new();
        for rec in log.open_dwells() {
            if !matches!(rec, ObservationRecord::Dwell(_)) {
                continue;
            }
            let read = hk_store::spans_from_records(
                std::slice::from_ref(&rec),
                &[],
                FreqRange::new(f64::NEG_INFINITY, f64::INFINITY),
            );
            // A notched dwell is three spans (T-595); the band is their hull.
            let mut band: Option<Tuned> = None;
            for sp in &read.spans {
                if sp.time.end <= sp.time.start {
                    continue;
                }
                let device = sp.device.as_str().to_owned();
                let b = band.get_or_insert_with(|| Tuned {
                    device,
                    f_lo_hz: sp.freq.lo_hz,
                    f_hi_hz: sp.freq.hi_hz,
                    since_ns: sp.time.start.as_unix_nanos(),
                    reach_ns: sp.time.end.as_unix_nanos(),
                    source: "open-dwell",
                });
                b.f_lo_hz = b.f_lo_hz.min(sp.freq.lo_hz);
                b.f_hi_hz = b.f_hi_hz.max(sp.freq.hi_hz);
                b.since_ns = b.since_ns.min(sp.time.start.as_unix_nanos());
                b.reach_ns = b.reach_ns.max(sp.time.end.as_unix_nanos());
            }
            let Some(b) = band else { continue };
            match dwells.get(&b.device) {
                Some(have) if have.since_ns >= b.since_ns => {}
                _ => {
                    dwells.insert(b.device.clone(), b);
                }
            }
        }
        for (d, t) in dwells {
            out.entry(d).or_insert(t);
        }
    }
    out
}

/// Follows the tune record for one subscription and states each move once.
#[derive(Debug, Default)]
pub struct ChangeWatch {
    last: BTreeMap<String, Tuned>,
    seq: u64,
}

impl ChangeWatch {
    /// A watch that starts from `now` — what the `subscribed` message states — so nothing already
    /// in it is reported as a change.
    pub fn new(now: BTreeMap<String, Tuned>) -> Self {
        Self { last: now, seq: 0 }
    }

    /// The bands as last seen.
    pub fn tuned(&self) -> impl Iterator<Item = &Tuned> {
        self.last.values()
    }

    /// Folds in a fresh reading and returns one `coverage_changed` per front end that **moved**
    /// (its band changed, or it appeared). A front end that kept its band, or is gone from the
    /// reading, produces nothing.
    pub fn step(&mut self, now: BTreeMap<String, Tuned>) -> Vec<Value> {
        let mut out = Vec::new();
        for (device, t) in now {
            let prev = self.last.get(&device);
            let moved = match prev {
                None => true,
                // A reading older than the one in hand is a lagging source, not a move back.
                Some(p) => t.since_ns >= p.since_ns && !t.same_band(p),
            };
            if moved {
                self.seq += 1;
                let (f_lo, f_hi) = prev.map_or((t.f_lo_hz, t.f_hi_hz), |p| {
                    (p.f_lo_hz.min(t.f_lo_hz), p.f_hi_hz.max(t.f_hi_hz))
                });
                out.push(json!({
                    "type": "coverage_changed",
                    "seq": self.seq,
                    "device": device,
                    "t": t.since_ns as f64 / 1e9,
                    "f_lo": f_lo,
                    "f_hi": f_hi,
                    "departed": prev.map(Tuned::band_json),
                    "arrived": t.band_json(),
                    "as_of_s": t.reach_ns as f64 / 1e9,
                    "source": t.source,
                }));
                self.last.insert(device, t);
            } else if prev.is_some_and(|p| t.since_ns >= p.since_ns) {
                self.last.insert(device, t);
            }
        }
        out
    }
}

/// The first message: the bands every front end is on as the watch starts.
fn subscribed_json(watch: &ChangeWatch) -> Value {
    json!({
        "type": "subscribed",
        "tuned": watch.tuned().map(Tuned::json).collect::<Vec<_>>(),
        "tick_ms": CHANGE_TICK.as_millis() as u64,
        "rule": "coverage_changed {f_lo, f_hi, t} is sent once per front-end MOVE: the union of the \
                 band it left and the band it arrived at, from the instant it arrived. Every tile \
                 intersecting [f_lo, f_hi] x [t, now] holds out-of-date coverage; nothing else did \
                 change. A provenance change that keeps the band sends nothing.",
    })
}

/// Serves one `/ws/tiles/changes` request (token already verified).
pub(crate) fn serve(
    stream: TcpStream,
    state: &ApiState,
    query: &Params,
    headers: &[(String, String)],
) {
    let Some(mut ws) = accept(stream, headers) else {
        return;
    };
    let Some(_slot) = FeedSlot::take(&state.change_feeds, MAX_CHANGE_FEEDS) else {
        return refuse(
            ws,
            &ApiError::new(
                503,
                format!("{MAX_CHANGE_FEEDS} change subscriptions are already open on this server"),
            ),
        );
    };
    if let Some((k, _)) = query.iter().find(|(k, _)| !ALLOWED.contains(&k.as_str())) {
        return refuse(ws, &ApiError::new(400, format!("unknown parameter `{k}`")));
    }
    let mut watch = ChangeWatch::new(tuned_now(state));
    if !send(&mut ws, &subscribed_json(&watch)) {
        return;
    }
    loop {
        for v in watch.step(tuned_now(state)) {
            if !send(&mut ws, &v) {
                return;
            }
        }
        if !peer_alive(&mut ws, CHANGE_TICK) {
            break;
        }
    }
    let _ = ws.get_mut().shutdown(Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(device: &str, lo: f64, hi: f64, since: i64) -> Tuned {
        Tuned {
            device: device.into(),
            f_lo_hz: lo,
            f_hi_hz: hi,
            since_ns: since,
            reach_ns: since + 1,
            source: "iq-ring",
        }
    }

    fn map(ts: &[Tuned]) -> BTreeMap<String, Tuned> {
        ts.iter().map(|x| (x.device.clone(), x.clone())).collect()
    }

    #[test]
    fn a_move_is_one_event_naming_both_bands_from_the_arrival() {
        let mut w = ChangeWatch::new(map(&[t("a", 100e6, 102e6, 0)]));
        assert!(w.step(map(&[t("a", 100e6, 102e6, 0)])).is_empty());
        // Same band, new segment (a gain step): not a move.
        assert!(w.step(map(&[t("a", 100e6, 102e6, 5_000)])).is_empty());
        let ev = w.step(map(&[t("a", 110e6, 112e6, 9_000_000_000)]));
        assert_eq!(ev.len(), 1, "{ev:?}");
        let e = &ev[0];
        assert_eq!(e["type"], "coverage_changed");
        assert_eq!(e["seq"], 1);
        assert_eq!(e["t"], 9.0);
        assert_eq!(e["f_lo"], 100e6);
        assert_eq!(e["f_hi"], 112e6);
        assert_eq!(e["departed"], json!({ "f_lo": 100e6, "f_hi": 102e6 }));
        assert_eq!(e["arrived"], json!({ "f_lo": 110e6, "f_hi": 112e6 }));
        // Stated once: the next reading of the same band is quiet.
        assert!(
            w.step(map(&[t("a", 110e6, 112e6, 9_000_000_000)]))
                .is_empty()
        );
    }

    #[test]
    fn appearing_is_a_change_vanishing_and_lagging_are_not() {
        let mut w = ChangeWatch::new(map(&[t("a", 100e6, 102e6, 10)]));
        let ev = w.step(map(&[t("a", 100e6, 102e6, 10), t("b", 5e6, 7e6, 20)]));
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0]["device"], "b");
        assert_eq!(ev[0]["departed"], Value::Null);
        assert!(
            w.step(map(&[t("b", 5e6, 7e6, 20)])).is_empty(),
            "a is gone: silence"
        );
        assert!(
            w.step(map(&[t("a", 100e6, 102e6, 10), t("b", 1e6, 3e6, 15)]))
                .is_empty(),
            "an older reading is a lagging source, not a move back"
        );
    }
}
