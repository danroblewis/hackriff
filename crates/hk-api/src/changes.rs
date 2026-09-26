//! `GET /ws/changes` — the **versioned change feed** (T-1065): the server says *what changed*, so a
//! client stops asking.
//!
//! # Why
//!
//! Measured in real Chrome on 2026-09-26: a thin client following the live edge with one pane and
//! nothing moving issued **7.3 requests a second** besides its WebSockets — `/api/outputs` and
//! `/api/pipelines` every second; `/api/control/state` (10 kB), `/api/annotations`, `/api/paths`,
//! `/api/tune-history`, `/api/frontend/events` and the survey coverage every two; `/api/inventory`
//! twice, `/api/timeline` and `/api/recordings` every five. A 24-event trackpad zoom produced 264
//! requests. Freezing the view saved nothing (6.7 req/s), because **none of those answers changes
//! between polls in the common case**: they were re-reads of an unchanged body, one new TCP
//! connection each, and the staging tunnel logged 8 622 `Unable to reach the origin service: EOF`
//! in fifteen minutes under them.
//!
//! A poll is a client asking "did it change?" of a server that already knows. This route answers
//! that question once, in the direction the knowledge flows.
//!
//! # The contract
//!
//! Each route in [`Change::ALL`] has a **version**: a counter, per server process, that the server
//! increments when the state behind that route is written. On connect the socket sends the whole
//! table once; after that it sends one
//!
//! ```json
//! {"type": "changed", "route": "/api/control/state", "version": 7, "t_s": 1790000000.123}
//! ```
//!
//! per route whose version moved, and **nothing at all while nothing changes**. The route key *is*
//! the path the client fetches, so nothing has to be mapped: `changed` on `/api/inventory` means
//! re-read `/api/inventory` (with whatever window the client is showing) and file the answer under
//! `version`.
//!
//! **Coalesced by construction.** The feed is not a queue of events; it is a *table of versions*
//! that a connection samples every [`TICK`] (250 ms). Twenty writes inside one tick advance the
//! counter twenty times and produce **one** message carrying the newest version — so a burst can
//! never amplify into a burst of messages, and the per-route ceiling is four messages a second
//! however hard the state is being written. Nothing is queued per connection and nothing is
//! buffered, so a slow reader cannot make the server hold history for it: it simply learns the
//! latest version a little later.
//!
//! **A version is an opaque monotonic number**, not a count of writes anyone should read meaning
//! into, and it is **per process**: a restart resets it (and the client reconnects and takes the
//! new snapshot, so it re-reads once). `0` means "not written since this server started".
//!
//! # What this feed does NOT report, and why the client keeps a slow poll
//!
//! A version moves when a **request writes** the state behind it ([`routes_for_write`], applied at
//! the one control-plane choke point every mutating route already passes through). It does **not**
//! move for state that grows because the radio is running:
//!
//! - new detections arriving in the inventory,
//! - the coverage map widening as the front end is swept by an already-started sweep,
//! - the capture window advancing, or a front-end event being judged,
//! - a re-plumb changing `run.segment` on `/api/control/state`.
//!
//! Those are producer-side events. [`ChangeFeed::bump`] is public precisely so a producer can
//! report them — it is the API the pipeline's ingest and T-1040's `coverage_changed` build on — but
//! **nothing calls it from the capture path today**, and a route with no producer wired is a route a
//! client must still poll (slowly) to see grow. Saying so here is the point: a feed that implied it
//! covered the live edge would be the "we have it but didn't render it" bug with the arrow
//! reversed. `/ws/tiles/rows` (T-468) already pushes the waterfall's own rows, and is not duplicated
//! here.
//!
//! # Cost
//!
//! A bump is one relaxed `fetch_add` on an atomic in an array — no lock, no allocation, no I/O — so
//! it is payable on any thread, including one that gates the ring. A connection costs one thread
//! blocked in `read` with a 250 ms timeout (the same shape as `/ws/tiles/rows`), which is why the
//! count is capped at [`MAX_CHANGE_FEEDS`] per server. An idle feed sends **no messages**; it sends
//! an unsolicited **ping** every [`PING_EVERY`] so an idle socket survives a proxy without any JSON
//! traffic at all.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use hk_model::Timestamp;
use serde_json::{Map, Value, json};

use crate::http::ApiState;
use crate::query::ApiError;
use crate::websock::{drop_socket, peer_alive, ping, refuse, send, upgrade};

/// How often a connection samples the version table. The **per-route ceiling on messages** is one
/// per tick: 4 a second, whatever the write rate.
pub const TICK: Duration = Duration::from_millis(250);

/// How often an **idle** feed pings, so a proxy does not reap a socket that is deliberately silent.
pub const PING_EVERY: Duration = Duration::from_secs(20);

/// Concurrent `/ws/changes` subscriptions per server (one thread each). A client needs exactly one;
/// this is headroom for a reload racing its predecessor's teardown, plus `hk` and a second page.
pub const MAX_CHANGE_FEEDS: usize = 8;

/// One route with a version. The variant's [`Change::route`] is the **path a client fetches**, so
/// the feed needs no mapping table on the client side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Change {
    /// `/api/control/state` — the 10 kB body the review found being re-read every 2 s.
    ControlState,
    /// `/api/navigation` — the achievable (centre, span) grid, which a device write re-derives.
    Navigation,
    /// `/api/coverage` — observed-versus-unobserved, derived from the tune history.
    Coverage,
    /// `/api/tune-history` — the recorded tune intervals.
    TuneHistory,
    /// `/api/timeline` — the capture window (retention, `t0..t1`).
    Timeline,
    /// `/api/recordings` — the persisted IQ recordings.
    Recordings,
    /// `/api/inventory` — the window-scoped Candidate/Confirmed lists.
    Inventory,
    /// `/api/annotations` — the user's own marks.
    Annotations,
    /// `/api/paths` — traced `(t, f)` paths.
    Paths,
    /// `/api/outputs` — output recordings.
    Outputs,
    /// `/api/pipelines` — running pipelines (and the recipes they run).
    Pipelines,
    /// `/api/frontend/events` — clipped whole-span steps.
    Frontend,
}

impl Change {
    /// Every route the feed carries, in wire order. The array's index is the variant's slot in
    /// [`ChangeFeed`]'s table, which is why it is written out rather than derived.
    pub const ALL: [Change; 12] = [
        Change::ControlState,
        Change::Navigation,
        Change::Coverage,
        Change::TuneHistory,
        Change::Timeline,
        Change::Recordings,
        Change::Inventory,
        Change::Annotations,
        Change::Paths,
        Change::Outputs,
        Change::Pipelines,
        Change::Frontend,
    ];

    /// The route this version belongs to — the path a client `GET`s.
    pub const fn route(self) -> &'static str {
        match self {
            Change::ControlState => "/api/control/state",
            Change::Navigation => "/api/navigation",
            Change::Coverage => "/api/coverage",
            Change::TuneHistory => "/api/tune-history",
            Change::Timeline => "/api/timeline",
            Change::Recordings => "/api/recordings",
            Change::Inventory => "/api/inventory",
            Change::Annotations => "/api/annotations",
            Change::Paths => "/api/paths",
            Change::Outputs => "/api/outputs",
            Change::Pipelines => "/api/pipelines",
            Change::Frontend => "/api/frontend/events",
        }
    }

    /// The variant for a route key, or `None` for a route the feed does not carry.
    pub fn from_route(route: &str) -> Option<Self> {
        Change::ALL.into_iter().find(|c| c.route() == route)
    }

    fn slot(self) -> usize {
        // `position` over ALL, not `self as usize`: the table's order is the declared one above,
        // so adding a variant in the middle cannot silently renumber a live server's versions.
        Change::ALL
            .iter()
            .position(|c| *c == self)
            .expect("every variant is in Change::ALL")
    }
}

/// The routes a **successful mutating request** to `path` changes the state behind.
///
/// This is the whole write side of the feed, as one pure function, applied once — in
/// [`crate::control::dispatch_device`], the choke point every mutating control-plane route already
/// passes through. It is deliberately **generous where a write may or may not have moved a derived
/// route**: a spurious re-read costs one request, a missed change leaves a client showing something
/// that is no longer true, and those are not the same mistake. So every device write bumps the
/// tuning's derived routes (the achievable grid, the tune history, the coverage that is computed
/// from it) whether or not this particular field reached them.
///
/// A path that maps to nothing bumps nothing: the feed carries [`Change::ALL`] and says so, and a
/// client polls what the feed does not carry. `changes_cover_every_listed_route` pins the mapping
/// against [`crate::http::ROUTES`] so a route cannot join the list without an answer here.
pub fn routes_for_write(path: &str) -> &'static [Change] {
    use Change::*;
    /// Moving the radio re-derives the tuning, the achievable grid, the recorded tune intervals and
    /// the coverage computed from them.
    const DEVICE: &[Change] = &[ControlState, Navigation, TuneHistory, Coverage];
    match path {
        "/api/control/center"
        | "/api/control/rate"
        | "/api/control/window"
        | "/api/control/gains"
        | "/api/control/bias_tee"
        | "/api/control/baseband_filter"
        | "/api/control/scan"
        | "/api/control/scan/stop" => DEVICE,
        // Display state changes what is shown, never what is captured (T-347's rule, on the wire).
        "/api/control/display" => &[ControlState],
        // Manual recording is `run.recording` on the state *and* a recording to list; its window is
        // the retained capture the timeline draws.
        "/api/control/record/start" | "/api/control/record/stop" => {
            &[ControlState, Recordings, Timeline]
        }
        "/api/outputs/record/start" | "/api/outputs/record/stop" => &[Outputs],
        // T-157: a clip of the ring becomes a persisted recording, which extends the audio horizon.
        "/api/iqbuffer/clip" => &[Recordings, Timeline],
        _ => prefixed(path),
    }
}

/// The prefix half of [`routes_for_write`]: the collection routes, whose ids are in the path.
fn prefixed(path: &str) -> &'static [Change] {
    use Change::*;
    let under = |p: &str| path == p || path.starts_with(&format!("{p}/"));
    if under("/api/annotations") {
        &[Annotations]
    } else if under("/api/paths") {
        &[Paths]
    } else if under("/api/pipelines") || under("/api/recipes") {
        &[Pipelines]
    } else if under("/api/inventory") || under("/api/clusters") {
        // A promote, a merge, a band edit — and a cluster promoted to an emitter — all change what
        // the window-scoped lists answer.
        &[Inventory]
    } else if under("/api/recordings") {
        &[Recordings]
    // Neither `/api/paths` nor `/api/recordings` has a mutating route today (a path is traced by
    // the pipeline, a recording is made by `/api/control/record/*` or `/api/iqbuffer/clip`). The
    // arms are declared anyway, and tested, so a mutating route added under either prefix later
    // cannot silently skip the feed and leave a client showing what it read before.
    } else {
        &[]
    }
}

/// The version table: one counter per [`Change`], shared by cloning the [`std::sync::Arc`] on
/// [`ApiState`].
///
/// A bump is a relaxed `fetch_add`; a read is a relaxed load. There is no subscriber list and no
/// per-connection queue — a connection *samples* this table (see the module docs), which is what
/// makes coalescing a property of the design rather than a timer someone has to get right.
#[derive(Debug, Default)]
pub struct ChangeFeed {
    versions: [AtomicU64; Change::ALL.len()],
    /// Open `/ws/changes` connections, against [`MAX_CHANGE_FEEDS`].
    feeds: AtomicUsize,
}

impl ChangeFeed {
    /// This route's current version. `0` until its first write.
    pub fn version(&self, c: Change) -> u64 {
        self.versions[c.slot()].load(Ordering::Relaxed)
    }

    /// Records that the state behind `c` was written; returns the new version.
    ///
    /// Cheap enough for any thread, including the capture thread: one atomic add, no allocation.
    pub fn bump(&self, c: Change) -> u64 {
        self.versions[c.slot()].fetch_add(1, Ordering::Relaxed) + 1
    }

    /// [`Self::bump`] for each of `cs` (the shape [`routes_for_write`] answers in).
    pub fn bump_all(&self, cs: &[Change]) {
        for c in cs {
            self.bump(*c);
        }
    }

    /// Every route's version, keyed by route.
    pub fn versions_json(&self) -> Value {
        let mut m = Map::new();
        for c in Change::ALL {
            m.insert(c.route().to_owned(), json!(self.version(c)));
        }
        Value::Object(m)
    }

    fn all(&self) -> [u64; Change::ALL.len()] {
        Change::ALL.map(|c| self.version(c))
    }
}

/// Counts open feeds against [`MAX_CHANGE_FEEDS`]; released on drop.
struct FeedSlot<'a>(&'a AtomicUsize);

impl<'a> FeedSlot<'a> {
    fn take(n: &'a AtomicUsize) -> Option<Self> {
        n.fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
            (v < MAX_CHANGE_FEEDS).then_some(v + 1)
        })
        .ok()
        .map(|_| Self(n))
    }
}

impl Drop for FeedSlot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn now_s() -> f64 {
    Timestamp::now().as_unix_nanos() as f64 / 1e9
}

/// The first message: the whole table, so a client that has just connected (or reconnected) knows
/// which of its cached bodies are stale without a `changed` for each of them.
fn versions_message(feed: &ChangeFeed) -> Value {
    json!({
        "type": "versions",
        "routes": feed.versions_json(),
        "tick_ms": TICK.as_millis() as u64,
        "t_s": now_s(),
    })
}

fn changed_message(c: Change, version: u64) -> Value {
    json!({
        "type": "changed",
        "route": c.route(),
        "version": version,
        "t_s": now_s(),
    })
}

/// Serves one `/ws/changes` connection (token already verified by [`crate::http`]).
pub(crate) fn serve(stream: std::net::TcpStream, state: &ApiState, headers: &[(String, String)]) {
    let Some(mut ws) = upgrade(stream, headers) else {
        return;
    };
    let feed = &*state.changes;
    let Some(_slot) = FeedSlot::take(&feed.feeds) else {
        return refuse(
            ws,
            &ApiError::new(
                503,
                format!("{MAX_CHANGE_FEEDS} change feeds are already open on this server"),
            ),
        );
    };
    // Sampled BEFORE the snapshot is sent, so a write that lands between the two is reported as a
    // `changed` rather than silently folded into a snapshot the client already had.
    let mut seen = feed.all();
    if !send(&mut ws, &versions_message(feed)) {
        return;
    }
    let mut last_ping = Instant::now();
    loop {
        if !peer_alive(&mut ws, TICK) {
            break;
        }
        let now = feed.all();
        let mut said_something = false;
        for (i, c) in Change::ALL.into_iter().enumerate() {
            if now[i] != seen[i] {
                seen[i] = now[i];
                if !send(&mut ws, &changed_message(c, now[i])) {
                    return;
                }
                said_something = true;
            }
        }
        // A message is itself proof the socket is alive; only a genuinely silent feed pings.
        if said_something {
            last_ping = Instant::now();
        } else if last_ping.elapsed() >= PING_EVERY {
            if !ping(&mut ws) {
                break;
            }
            last_ping = Instant::now();
        }
    }
    drop_socket(&mut ws);
}

/// Is this GET answered with a validator (`ETag`/`If-None-Match` → `304`)?
///
/// `/api/control/state` alone, for now, and for the reason the review measured: 10 kB every two
/// seconds, byte-identical almost every time. The tag is **content-derived**, so a body that really
/// did change (a re-plumb moved `run.segment`, a sweep advanced `scan`) still answers `200` with
/// the new bytes — the validator can never serve a stale state, only skip an identical one.
pub fn validated(path: &str) -> bool {
    path == Change::ControlState.route()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::ROUTES;

    #[test]
    fn every_route_is_a_real_get_route_and_keys_are_unique() {
        for c in Change::ALL {
            assert!(
                ROUTES.iter().any(|(m, p)| *m == "GET" && *p == c.route()),
                "{:?}: {} is not a GET route the server serves",
                c,
                c.route()
            );
            assert_eq!(Change::from_route(c.route()), Some(c));
        }
        let mut keys: Vec<&str> = Change::ALL.iter().map(|c| c.route()).collect();
        keys.sort_unstable();
        let n = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), n, "route keys must be unique");
        assert_eq!(Change::from_route("/api/status"), None);
    }

    #[test]
    fn slots_are_distinct_and_within_the_table() {
        let mut slots: Vec<usize> = Change::ALL.iter().map(|c| c.slot()).collect();
        slots.sort_unstable();
        assert_eq!(slots, (0..Change::ALL.len()).collect::<Vec<_>>());
    }

    #[test]
    fn a_bump_moves_exactly_one_version() {
        let feed = ChangeFeed::default();
        for c in Change::ALL {
            assert_eq!(feed.version(c), 0, "{c:?} starts unwritten");
        }
        assert_eq!(feed.bump(Change::Inventory), 1);
        assert_eq!(feed.bump(Change::Inventory), 2);
        assert_eq!(feed.version(Change::Inventory), 2);
        for c in Change::ALL.into_iter().filter(|c| *c != Change::Inventory) {
            assert_eq!(feed.version(c), 0, "{c:?} must not move with another route");
        }
        assert_eq!(feed.versions_json()["/api/inventory"], json!(2));
    }

    #[test]
    fn a_device_write_reaches_the_tuning_and_everything_derived_from_it() {
        for p in [
            "/api/control/center",
            "/api/control/rate",
            "/api/control/window",
            "/api/control/gains",
            "/api/control/bias_tee",
            "/api/control/baseband_filter",
            "/api/control/scan",
            "/api/control/scan/stop",
        ] {
            let r = routes_for_write(p);
            for want in [
                Change::ControlState,
                Change::Navigation,
                Change::TuneHistory,
                Change::Coverage,
            ] {
                assert!(r.contains(&want), "{p} must bump {want:?}");
            }
        }
        // A view change is not a device change: it moves the state body and nothing else.
        assert_eq!(
            routes_for_write("/api/control/display"),
            &[Change::ControlState]
        );
    }

    #[test]
    fn collection_writes_map_by_prefix_including_their_ids() {
        assert_eq!(routes_for_write("/api/annotations"), &[Change::Annotations]);
        assert_eq!(
            routes_for_write("/api/annotations/01HZZZ"),
            &[Change::Annotations]
        );
        assert_eq!(routes_for_write("/api/pipelines"), &[Change::Pipelines]);
        assert_eq!(
            routes_for_write("/api/pipelines/p1/channels"),
            &[Change::Pipelines]
        );
        assert_eq!(routes_for_write("/api/recipes/r1"), &[Change::Pipelines]);
        assert_eq!(
            routes_for_write("/api/inventory/01HZZZ/promote"),
            &[Change::Inventory]
        );
        assert_eq!(
            routes_for_write("/api/clusters/c1/promote"),
            &[Change::Inventory]
        );
        assert_eq!(
            routes_for_write("/api/outputs/record/start"),
            &[Change::Outputs]
        );
        // Declared before their first writer exists (see `prefixed`), so one cannot be added
        // without the feed.
        assert_eq!(routes_for_write("/api/paths/p1"), &[Change::Paths]);
        assert_eq!(
            routes_for_write("/api/recordings/r1"),
            &[Change::Recordings]
        );
        // A prefix is a path segment, never a string prefix: `/api/annotationsX` is not one.
        assert!(routes_for_write("/api/annotationsX").is_empty());
        // A route the feed does not carry bumps nothing at all — it does not fall back to "all".
        assert!(routes_for_write("/api/ml/models/m/mode").is_empty());
        assert!(routes_for_write("/api/playback").is_empty());
    }

    #[test]
    fn only_control_state_is_validated_today() {
        assert!(validated("/api/control/state"));
        for c in Change::ALL
            .into_iter()
            .filter(|c| *c != Change::ControlState)
        {
            assert!(!validated(c.route()), "{c:?} is not validated yet");
        }
    }

    #[test]
    fn the_feed_route_is_in_the_table() {
        assert!(
            ROUTES
                .iter()
                .any(|(m, p)| *m == "GET" && *p == "/ws/changes")
        );
    }

    #[test]
    fn the_snapshot_names_every_route_and_the_tick() {
        let feed = ChangeFeed::default();
        feed.bump_all(routes_for_write("/api/control/center"));
        let m = versions_message(&feed);
        assert_eq!(m["type"], json!("versions"));
        assert_eq!(m["tick_ms"], json!(250));
        for c in Change::ALL {
            assert!(m["routes"][c.route()].is_u64(), "{}: {m}", c.route());
        }
        assert_eq!(m["routes"]["/api/control/state"], json!(1));
        assert_eq!(m["routes"]["/api/inventory"], json!(0));
        let c = changed_message(Change::Coverage, 3);
        assert_eq!(
            (c["type"].clone(), c["route"].clone(), c["version"].clone()),
            (json!("changed"), json!("/api/coverage"), json!(3))
        );
        assert!(c["t_s"].as_f64().is_some_and(|t| t > 1.7e9));
    }
}
