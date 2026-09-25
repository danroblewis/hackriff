//! `GET /api/tiles` — one tile of the unified surface, addressed by **independent**
//! `(level_f, level_t)` (T-438, `docs/16` §7 step 5 re-scoped by §8).
//!
//! # Why one route and not two
//!
//! `docs/16` §7 step 5's own reason: the panes, the minimap and the live edge are **projections of
//! the same pyramid**, so one route is what stops them ever disagreeing on one screen. Under §8
//! that argument gets stronger, not weaker — there is no live-versus-history split left to keep
//! consistent, only viewports at different levels onto one surface.
//!
//! # The addressing, and what `device` and `scheme` do in it
//!
//! The key is `(device, scheme, level_f, level_t, f_index, t_index)`. §8.3 named only the last
//! four; §6.3 already required `device`, and retrofitting a key is the expensive kind of change,
//! so both are here from the start.
//!
//! - **`scheme` is the lattice the address is expressed in — and, since T-439, which store answers
//!   it.** The two cannot come apart: asking for the view lattice while reading scheme 1's ladder
//!   is exactly the gap T-438 left, where the coarse nodes had nothing behind them and fell back on
//!   a level whose time cell is a day. [`tile_store`] is the one mapping, and a server with no view
//!   pyramid open behaves as it did before.
//!   `scheme=view` (the default) is the de-welded view lattice: node `(0, 0)` is the
//!   open pyramid's **own level-0 cell** and each axis doubles independently, so every
//!   `(level_f, level_t)` in range is a node. `scheme=<n>` addresses a store scheme's own ladder
//!   through [`Geometry::axes_of`]/[`Geometry::level_at`] — and a **welded ladder is the diagonal**
//!   of that lattice, so `level_at(3, 0)` is `None` and this route answers *this scheme has no such
//!   node* (404) rather than snapping to a level whose time cell is a day (T-434).
//!   `scheme=overview` (T-505) is **the second tier**: the same de-welded construction anchored at
//!   the *spectrum-history* pyramid's level-0 cell and answered by that pyramid, so a
//!   wide-and-long viewport has a source whose cells are absolutely coarse. See
//!   [`TileLattice::overview`] for why one lattice cannot be both, measured.
//! - **`device` is whose coverage decides this tile's grey.** Coverage is device-local
//!   (T-259/T-305, §6.3), so it belongs in the key and not in a cell. `any` is the union and keeps
//!   `"named": false`, so a merged plane can never wear one radio's identity.
//!
//! **The view lattice's floor is the store's, not `docs/16` §6.2's.** §6.2 put node (0, 0) at
//! 100 kHz × **128 s** against a 120 s IQ retention — the entire live view inside one time cell,
//! and every realistic pane finer than it, so `level_t` pinned at 0 and the de-welding bought
//! nothing on the axis it was introduced for (T-437 finding F1). Anchoring at the open pyramid's
//! level-0 cell is that fix, and it is the geometry the spike actually ran on.
//!
//! # Where the tiles come from (T-439)
//!
//! From the **live chain**, and from nothing else. `hk_pipeline::history` folds each history frame
//! into the view pyramid's level 0 as it folds it into scheme 1, so the finest node *is* the
//! growing edge where hardware is currently tuned — `docs/16` §8.1's leap, which is why this route
//! has no live-specific branch and no live-versus-history seam to keep consistent. A tile at that
//! edge covers exactly what was sampled: the cells the front end's band and the frame's own
//! duration reach, and no others (T-406's rule). The newest cells of a growing tile are routinely
//! *observed but not yet measured* — the radio is demonstrably tuned there and the fold has not
//! caught up — and that is neither grey nor T-423's `"unknown"`: `coverage` says observed, this
//! route's `grid` has no cell yet, and the difference is visible because the two planes are served
//! separately.
//!
//! # How this avoids `/api/history`'s budget-as-level-selector defect (F2)
//!
//! T-437 measured it: same window, same band, only `max_f` changed — `max_f=384` served
//! 38 784/38 784 cells observed, `max_f=256` served 384/576 (**67 %**). Tightening the *frequency*
//! budget 1.5× cost **34× of time resolution and greyed a third of the window**. That is a
//! grey-honesty violation caused by **level choice**, which `docs/16` §4 does not name: §4 guards
//! the fold, and the fold is fine — here a cell reads *unobserved* while level 0 holds the
//! measurement.
//!
//! This route cannot express that bug, because **it has no caller-supplied per-axis budget at
//! all**:
//!
//! 1. The tile's grid is always exactly `cells × cells` laid on the tile's own extent. The address
//!    *is* the budget, so `level_f` and `level_t` are structurally independent — changing one
//!    cannot move the other's cell size by so much as a rounding (asserted in
//!    `changing_one_axis_level_never_moves_the_other_axis_cell`).
//! 2. The store level is chosen **finest-affordable-first**, never coarsest-adequate. Folding a
//!    finer level onto the tile's grid can never grey a cell the finer level holds — a fold is a
//!    max and a sum, so an output cell is observed if *any* source cell in it was. Only the
//!    opposite direction (a source coarser than the tile's cell, which **replicates** a measured
//!    value) is a claim, and it is stated per axis in `resolution.fold` and downgrades the honesty
//!    tier to `survey-overview`.
//! 3. Candidates are ordered by **cell area**, explicitly, never by level index. T-434's warning:
//!    index order is a coarseness order only for a ladder — in a lattice node (1, 0) outranks
//!    (0, 3) in index while being *finer* in time.
//! 4. When the finest affordable level holds nothing, the read walks **coarser** through the
//!    remaining candidates (T-426's rule, in the direction this route's preference makes
//!    meaningful: the byte budget evicts the finest tiles first, §5.5). `resolution.answered`
//!    reports the level that **actually answered**, and `resolution.tried` every level consulted —
//!    a silent fallback would trade one lie for another.
//!
//! # Cost, and the two caps
//!
//! T-437 measured rendering at p95 2.2 ms for 48 panes and tile **production** at ~500 ms per
//! tile — three orders of magnitude apart. Production is what this route is designed against.
//!
//! - **Work per tile** is bounded by [`TILE_MAX_TOTAL_SOURCE_CELLS`], and **one lock hold** by
//!   [`TILE_MAX_SOURCE_CELLS`]: the read is chunked into whole output rows, re-acquiring the
//!   history lock per chunk, so a tile fan-out at the live edge can never lock ingest out for a
//!   whole tile (§5.5 cap 3, the report builder's discipline).
//! - **Concurrency** is bounded by [`TILE_MAX_IN_FLIGHT`] ([`TileSlot`]). Over the cap the answer
//!   is `503` naming the cap, not a queue that grows until ingest starves — **unless the tile is
//!   sealed and already in the hot-tile cache** (T-581): the cap bounds *production*, and a cached
//!   answer produces nothing, so it is served without a slot ([`hot_hit_unslotted`]).
//!
//! # The coverage plane is a table of distinct planes, run-length encoded (T-467)
//!
//! `coverage` here is **not** `/api/coverage`'s per-cell form, and that is the single largest cost
//! ever measured on this route. The per-cell form serialises
//! `{"state":…,"duty":…,"observed_s":…,"last_s":…,"spans":…,"center_hz":…,"sample_rate_hz":…}` —
//! about 146 B — for each of 65 536 cells, and this route carried **two** such planes, `any` and
//! `devices[0]`, byte-identical on a one-device server. Measured against the demo backend that was
//! 99 % of a 19.34 MB tile body, to deliver the one field `ui/src/surface/tile.ts` reads.
//!
//! [`crate::coverage::TileOverlay`] serves each **distinct** plane once, as a run-length encoding
//! of per-cell state codes over an alphabet served beside it. Measured in-process on the same
//! 256 × 256 grid: 19 818 236 B → 2 906 B. The three states are untouched — `unobserved` is still
//! its own code, `unknown` (T-423) still its own, and no cell on the plane carries a measurement
//! key of any kind, so there is nothing on it a client could read as a level of zero. The per-cell
//! sampling detail is a **hover** question about one cell and `/api/coverage` still answers it;
//! `/api/timeline`'s overlay is unchanged.
//!
//! # `max_db` is served as binary16, because that is what it becomes (T-533)
//!
//! With the coverage plane compacted, what was left of a live tile's body was the measurement
//! itself, spelled as JSON decimal text: **1 197 118 B of a 1 878 289 B tile — 64 %** — for 65 536
//! values whose destination is an **R16F texture** (`ui/src/surface/surface.ts`). Seventeen
//! significant digits are transmitted, then eleven bits of them are kept. So `?planes=f16` serves
//! that one plane as base64 of little-endian IEEE binary16 ([`f16_bits`]), and the wire states its
//! own type, byte order, scale and absent-value rather than leaving a reader to infer them.
//!
//! **Only that plane.** `occupancy_max`, `coverage` and `frames` stay JSON arrays, because for them
//! JSON is *smaller*: measured on the same tile, `frames` is 131 073 B as text (two distinct values
//! over 65 536 cells) against 349 528 B as base64 `u32`, and the other two lose likewise. A "pack
//! everything" mode would have grown three planes to shrink one. See [`Planes`].
//!
//! Measured through the route, one address, four spellings back to back on a full 256 x 256 live
//! tile: **1 879 209 B** as JSON, **856 178 B** packed, **244 012 B** JSON gzipped and **117 382 B**
//! packed *and* gzipped (`accept-encoding`, T-533, [`crate::http`]) — **16x**. Both levers pay and
//! neither subsumes the other: JSON decimal text is high-entropy by construction, so compressing it
//! is not the same as not sending it. `cost.build_ms` did not move (20.0 -> 20.4 ms), which is the
//! honest half of the result: the route's own work was never the float formatting, so on a loopback
//! link the body was not what a refetch was waiting for. It is what a tunnel, a phone or a second
//! machine waits for, and it is what the browser parses.
//!
//! # What a tile never carries
//!
//! Emitters (§5.3). Identity gating is per-caller and a tile is not; a sealed tile is immutable and
//! an emitter set never is. The coarse-zoom form is a **count per cell**, served separately by
//! `GET /api/tiles/events` so the counts stay live while the tile stays cacheable.
//!
//! | Method | Path | Query | Answers |
//! |---|---|---|---|
//! | GET | `/api/tiles` | `?level_f&level_t&f_index&t_index[&scheme][&device][&cells][&planes]` | `{key, extent, axes, grid, coverage, resolution, cost}` |
//! | GET | `/api/tiles/events` | the same address | `{key, extent, counts, total, rule}` |

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hk_model::{FreqRange, TimeRange, Timestamp};
use hk_store::history::Geometry;
use hk_store::{Overview, OverviewCell, RegionQuery, Resolution};
use serde_json::{Value, json};

use crate::http::ApiState;
use crate::navigation::DetailSource;
use crate::query::{ApiError, Params, Region, bad, count, param};

/// Cells on each edge of a tile — `docs/16` §6.2's uniform 256 × 256.
///
/// Uniformity is what makes §5.5's **count**-based client budget an honest proxy for bytes, and
/// 256 × 256 is also exactly [`hk_store::coverage::MAX_COVERAGE_GRID_CELLS`], so a tile's
/// record-derived coverage plane is always laid on the tile's own axes and never silently reduced.
pub const TILE_CELLS: usize = 256;
/// Smallest tile edge accepted. A smaller tile is a test or a probe, not the canvas's unit.
pub const MIN_TILE_CELLS: usize = 8;

/// Source cells read under **one** history lock hold.
///
/// The tile read is chunked into whole output rows so no single hold exceeds this: an unbounded
/// fan-out at the live edge taking the history lock for a whole tile is exactly the failure the
/// report builder's ≤ 256-row chunks exist to prevent (`docs/16` §5.5 cap 3).
pub const TILE_MAX_SOURCE_CELLS: usize = crate::query::MAX_API_CELLS;

/// Source cells read for **one whole tile**, across every chunk.
///
/// This is the work bound that decides which store levels are affordable. It is deliberately a
/// different number from [`TILE_MAX_SOURCE_CELLS`]: that one bounds a lock hold, this one bounds a
/// request. Four chunk-loads is the widest read that still finishes well inside a tile's budget on
/// the measured per-cell cost (`docs/16` §6.4).
pub const TILE_MAX_TOTAL_SOURCE_CELLS: usize = 4 * TILE_MAX_SOURCE_CELLS;

/// How deep the **shadow search** may read, in rows across the tile's own columns (T-523).
///
/// A tile's budget is `cells × SHADOW_SEARCH_ROWS`, which is [`TILE_MAX_SHADOW_SOURCE_CELLS`] at
/// the route's own 256-cell unit. Rows rather than an area, so the *reach* of the search is the
/// same for a probe-sized tile as for a full one — the quantity the shadow is about is "how far
/// back can this look", and it should not depend on how many pixels someone asked for.
///
/// # Why the shadow is not budgeted like a tile read
///
/// T-519 gave the last-known search the tile read's own two budgets, on the reasoning that "the
/// shadow can at most double a tile's work". On the **full** path that is true and harmless. On
/// T-461's **coverage short-circuit** it is neither: that path exists precisely because the answer
/// is already known from the coverage plane and the pyramid read is 65 536 source cells spent to
/// confirm it — and then the shadow spent up to 2 000 000 on the same tile. The route's cheapest
/// answer became one of its most expensive, and the user met it as `502`s from the tunnel during a
/// zoom, where the burst asks for the same place at every level at once.
///
/// The right scale is the **tile**, not the store: the shadow decorates a `cells × cells` grid with
/// at most `cells` column values, so a few hundred rows across those columns is the most it can be
/// worth — 512, two per row of a full tile. What a tighter budget costs is stated by [`hk_store::Pyramid::last_known_search`] and is exactly what
/// makes it safe: the search plans fine-to-coarse and **skips a stage it cannot afford, leaving the
/// next coarser one to cover that window** — so the bound is paid in the shadow's frequency/time
/// *resolution*, which the wire reports per run (`shadow.sources[].level`), never in reach and
/// never in a silently-missing run. A window it genuinely could not read is reported in
/// `shadow.search.unsearched`, as it was before.
///
/// 512 rows is the number [`hk_store::Pyramid::last_known_search`]'s own documentation is written
/// against — *"the whole retained horizon costs a few hundred rows per column"* — because the ladder
/// is fine-to-coarse: 512 rows spread over the chain reach days, not 512 seconds.
pub const SHADOW_SEARCH_ROWS: usize = 512;

/// Source cells the shadow search may read for one tile of the route's own unit ([`TILE_CELLS`]),
/// and the ceiling on the per-tile budget computed in [`shadow`]. See [`SHADOW_SEARCH_ROWS`].
pub const TILE_MAX_SHADOW_SOURCE_CELLS: usize = TILE_CELLS * SHADOW_SEARCH_ROWS;

/// Tile reads in flight at once (`docs/16` §5.5 cap 3: **server backpressure**, not a browser's
/// connection limit).
///
/// Chosen against `hk-store`'s lock behaviour rather than against a client's appetite: the history
/// store is behind one mutex, so concurrent tile reads serialise on it anyway, and the only thing
/// a deeper queue buys is a longer stretch during which ingest is competing for that mutex. Four
/// keeps a pan's burst moving while leaving the lock free most of the time. Over the cap the answer
/// is `503`, which a client retries — §5.5's cap (1) is LIFO with viewport cancellation on the
/// client, and a refusal is what lets it cancel rather than wait.
///
/// **It is a PRODUCER cap** (T-581). A sealed tile the hot-tile cache (T-572) already holds is
/// answered even with every slot out, because answering it takes no pyramid read: before that, a
/// couple of slow coarse tiles holding the cap turned every re-poll of an already-drawn screen into
/// a `503`, and the client halves its operating limit on each (T-450: 26 tiles resident against
/// 17 268 cancelled in 75 s). The number itself is unchanged: every read that *does* produce still
/// takes the one history mutex per chunk, so raising it would only lengthen the stretch ingest
/// competes for that mutex — the store's lock was not changed, so neither is the cap.
pub const TILE_MAX_IN_FLIGHT: usize = 4;

/// The widest tile the view lattice needs: the whole 1 MHz–6 GHz device range in two tiles
/// (`docs/16` §6.2's V7, kept while its floor is replaced).
const VIEW_MAX_TILE_HZ: f64 = 3.0e9;
/// The tallest tile the view lattice needs: a month of retention in one tile (§6.2's V7).
const VIEW_MAX_TILE_NS: i64 = 30 * 86_400 * 1_000_000_000;
/// Hard cap on axis levels, so a misconfigured floor cannot produce an unbounded axis.
const MAX_VIEW_LEVELS: usize = 32;

/// How long a client that has stopped asking for tiles stays in the share table (T-630).
///
/// A client identity here is **declared**, not a connection: a browser tab makes its tile reads
/// over a pool of connections and would otherwise be several "clients". So the server is never
/// told when one goes away — a closed tab, a crashed browser, a `curl` that was `^C`'d — and a
/// share table that only grew would hand every surviving client an ever-smaller share of the cap,
/// which is the leak shape T-454 already paid for once with slots. The table is therefore a
/// **cache of who is asking now**: an entry with no slots out and no request inside this window is
/// forgotten, so the share of a client that disappears returns to the ones still here without
/// anybody telling the server anything. Slots themselves cannot leak either way — a [`TileSlot`]
/// decrements its client's counter on `Drop` even if the entry has since been evicted, because it
/// holds the counter rather than a key into the table.
pub const TILE_CLIENT_IDLE: Duration = Duration::from_secs(10);

/// Client identities tracked at once. Past this the least-recently-seen idle entry is dropped, and
/// if every entry is busy a new identity is served from the shared anonymous bucket — degrading to
/// the pre-T-630 first-come-first-served behaviour rather than growing without bound.
pub const TILE_CLIENT_MAX: usize = 64;

/// The bucket every request that declares no `client` shares (`curl`, the CLI, an old client).
/// They compete with each other exactly as they did before T-630, and as one client against the
/// declared ones — a caller that wants a share of its own says who it is.
pub const ANONYMOUS_CLIENT: &str = "-";

/// The `client` value this route will keep: short, printable, and its own.
fn client_id(q: &Params) -> String {
    let raw = param(q, "client").unwrap_or_default();
    let ok = !raw.is_empty()
        && raw.len() <= 64
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
    if ok {
        raw.to_string()
    } else {
        ANONYMOUS_CLIENT.to_string()
    }
}

/// One client's place in the share table.
#[derive(Debug)]
struct TileClient {
    id: String,
    /// Slots this client holds. An [`Arc`] so a [`TileSlot`] outliving the table entry still
    /// releases into the right counter.
    held: Arc<AtomicUsize>,
    last_seen: Instant,
    /// Has this client ever been *served* a tile? A client with nothing on screen yet is the one
    /// case this route treats as urgent (see [`TileAdmission::acquire`]).
    served: bool,
}

/// What admission decided, so the wire can state it rather than leave a client to infer it.
#[derive(Debug, Clone, Copy)]
pub struct Share {
    /// Slots this client may hold at once: `ceil(cap / clients)` under the fair share, the whole
    /// cap when it is off.
    pub share: usize,
    /// Clients counted when that was computed (this one included).
    pub clients: usize,
    /// Was a slot being held back for a client that has nothing on screen yet?
    pub reserved: usize,
    /// Slots out across all clients at the moment of the decision.
    pub in_flight: usize,
    /// Is the fair share in force at all (`HK_TILE_FAIR_SHARE=off` turns it off — the
    /// first-come-first-served route this ticket replaced, kept so the test that proves the share
    /// matters has something to go red against).
    pub fair: bool,
}

/// **Who may have one of the route's four slots, and why that is not first-come-first-served**
/// (T-630).
///
/// [`TILE_MAX_IN_FLIGHT`] is ingest backpressure and stays exactly what it was. What changes is
/// *whose* request meets it. Measured before this existed: while one tab enumerated a wide
/// viewport it held all four slots continuously — it re-asks the instant one frees — so a second
/// tab's **first** request, the one it cannot start without, competed on equal terms with the
/// thousandth request of a tab that is already drawn. That second tab booted in 8.2 s and 11.7 s
/// after 7 refusals in the runs that worked, and twice did not boot at all.
///
/// Raising the cap would move that failure rather than fix it, and would spend capacity on the
/// capture thread that T-453 measured is paid whether or not anyone is looking. So the cap is
/// unchanged and the *policy* is two rules:
///
/// 1. **A share of the budget per client.** `ceil(cap / clients)`, so two clients get two slots
///    each and a third client is guaranteed one. A client's own greed can no longer reach past its
///    share, however fast it re-asks — which is what makes the slots a newcomer needs appear
///    without anybody yielding them politely.
/// 2. **Priority by what the request *is*.** A client that has never been served a tile is
///    bootstrapping: it is asking for first paint, not for fill. While one exists, clients that are
///    already drawn are admitted only up to `cap - 1`, so the slot the newcomer needs is there on
///    its *next* attempt instead of after a queue of an already-drawn client's reads. This is
///    T-457's visible-fetch precedence and T-459's "no visible fetch is starved", at the one place
///    where the competing fetches belong to different clients. The reserve costs nothing when
///    nobody is bootstrapping, which is almost always: it is armed by the newcomer's own first
///    (refused) request and disarmed by its first success.
///
/// A refusal still names its numbers, so a client adopts its share instead of guessing (the
/// refusal is also how a client learns the share shrank because someone else arrived).
#[derive(Debug)]
pub struct TileAdmission {
    in_flight: Arc<AtomicUsize>,
    clients: Mutex<Vec<TileClient>>,
    fair: bool,
    /// Reads admission refused that were answered anyway from the hot-tile cache, holding no slot
    /// (T-581) — each one a `503` the pre-T-581 route would have issued.
    hot_answers: AtomicUsize,
}

impl Default for TileAdmission {
    fn default() -> Self {
        // Off is the pre-T-630 route, first-come-first-served, kept ONLY as the red baseline the
        // fair-share e2e is proved against (`ui/e2e/surface-contention.e2e.mjs`). Nothing in the
        // product sets it.
        let fair = !matches!(
            std::env::var("HK_TILE_FAIR_SHARE")
                .unwrap_or_default()
                .as_str(),
            "off" | "0" | "false"
        );
        Self {
            in_flight: Arc::new(AtomicUsize::new(0)),
            clients: Mutex::new(Vec::new()),
            fair,
            hot_answers: AtomicUsize::new(0),
        }
    }
}

impl TileAdmission {
    /// Slots out across every client.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    /// Refused reads answered from the hot-tile cache without a slot (T-581).
    pub fn hot_answers(&self) -> usize {
        self.hot_answers.load(Ordering::Acquire)
    }

    /// Is the fair share in force?
    pub fn fair(&self) -> bool {
        self.fair
    }

    fn share_for(fair: bool, clients: usize) -> usize {
        if !fair {
            return TILE_MAX_IN_FLIGHT;
        }
        TILE_MAX_IN_FLIGHT.div_ceil(clients.max(1)).max(1)
    }

    /// Forget clients that hold nothing and have not asked inside [`TILE_CLIENT_IDLE`].
    fn sweep(clients: &mut Vec<TileClient>, now: Instant) {
        clients.retain(|c| {
            c.held.load(Ordering::Acquire) > 0 || now.duration_since(c.last_seen) < TILE_CLIENT_IDLE
        });
    }

    /// Take a slot for `client`, or say why not. Either way the client is now *known*, including
    /// when it was refused — that is what shrinks everyone else's share to make room for it.
    pub fn acquire(self: &Arc<Self>, client: &str) -> Result<TileSlot, Share> {
        let now = Instant::now();
        let mut clients = self.clients.lock().expect("tile client table");
        Self::sweep(&mut clients, now);
        if !clients.iter().any(|c| c.id == client) {
            if clients.len() >= TILE_CLIENT_MAX {
                // Drop the coldest idle entry to make room; if every tracked client is busy, this
                // request joins the anonymous bucket rather than growing the table.
                let cold = clients
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.held.load(Ordering::Acquire) == 0)
                    .min_by_key(|(_, c)| c.last_seen)
                    .map(|(i, _)| i);
                match cold {
                    Some(i) => {
                        clients.swap_remove(i);
                    }
                    None => return self.acquire_known(&mut clients, ANONYMOUS_CLIENT, now),
                }
            }
            clients.push(TileClient {
                id: client.to_string(),
                held: Arc::new(AtomicUsize::new(0)),
                last_seen: now,
                served: false,
            });
        }
        self.acquire_known(&mut clients, client, now)
    }

    fn acquire_known(
        self: &Arc<Self>,
        clients: &mut [TileClient],
        client: &str,
        now: Instant,
    ) -> Result<TileSlot, Share> {
        let n = clients.len().max(1);
        let share = Self::share_for(self.fair, n);
        let Some(me) = clients.iter_mut().find(|c| c.id == client) else {
            // The table was full and every entry busy, and even the anonymous bucket is not in it:
            // refuse rather than grow. Nothing is lost — the caller retries, and by then a slot has
            // freed and an entry with it.
            return Err(Share {
                share,
                clients: n,
                reserved: 0,
                in_flight: self.in_flight(),
                fair: self.fair,
            });
        };
        me.last_seen = now;
        let served = me.served;
        let held = Arc::clone(&me.held);
        // The reserve is for a client with nothing on screen yet, so it is never held against one.
        let reserved = usize::from(
            self.fair
                && served
                && clients.iter().any(|c| {
                    c.id != client
                        && !c.served
                        && now.duration_since(c.last_seen) < TILE_CLIENT_IDLE
                }),
        );
        let mut decision = Share {
            share,
            clients: n,
            reserved,
            in_flight: self.in_flight(),
            fair: self.fair,
        };
        let ceiling = TILE_MAX_IN_FLIGHT.saturating_sub(reserved).max(1);
        if self.fair && held.load(Ordering::Acquire) >= share {
            return Err(decision);
        }
        // The global cap is still the one that protects ingest; the share only ever narrows it.
        let mut seen = self.in_flight.load(Ordering::Acquire);
        loop {
            if seen >= ceiling {
                decision.in_flight = seen;
                return Err(decision);
            }
            match self.in_flight.compare_exchange_weak(
                seen,
                seen + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(nowv) => seen = nowv,
            }
        }
        held.fetch_add(1, Ordering::AcqRel);
        decision.in_flight = seen + 1;
        Ok(TileSlot {
            global: Arc::clone(&self.in_flight),
            held,
            admission: Arc::clone(self),
            client: client.to_string(),
            decision,
        })
    }

    /// Mark a client as *drawn*: it has been served a tile, so it no longer arms the reserve.
    fn mark_served(&self, client: &str) {
        let mut clients = self.clients.lock().expect("tile client table");
        if let Some(c) = clients.iter_mut().find(|c| c.id == client) {
            c.served = true;
        }
    }
}

/// One tile read in flight, held by a named client. Dropping it releases the slot **and** the
/// client's share of it, so no error path can leak either.
///
/// The counters live on [`TileAdmission`] (which lives on [`ApiState`]), not in a `static`: two
/// servers in one test process must not share a cap, and a cap that leaks across tests is a cap
/// nobody can assert.
#[derive(Debug)]
pub struct TileSlot {
    global: Arc<AtomicUsize>,
    held: Arc<AtomicUsize>,
    admission: Arc<TileAdmission>,
    client: String,
    decision: Share,
}

impl TileSlot {
    /// Slots currently out across every client, including this one.
    pub fn in_flight(&self) -> usize {
        self.global.load(Ordering::Acquire)
    }

    /// What this client was admitted under.
    pub fn share(&self) -> Share {
        self.decision
    }

    /// Slots this client holds, including this one.
    pub fn held(&self) -> usize {
        self.held.load(Ordering::Acquire)
    }

    /// The client this read belongs to.
    pub fn client(&self) -> &str {
        &self.client
    }

    /// This client has now been served a tile, so it is drawn and no longer arms the bootstrap
    /// reserve. Called on the answer, never on the request: the point of the reserve is a client
    /// with nothing on screen, and a refused read put nothing on screen.
    pub fn mark_served(&self) {
        self.admission.mark_served(&self.client);
    }
}

impl Drop for TileSlot {
    fn drop(&mut self) {
        self.held.fetch_sub(1, Ordering::AcqRel);
        self.global.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The refusal served when admission says no, so the body states the numbers rather than only the
/// status. `limit` is the server-wide cap (unchanged, and what pre-T-630 clients parse); `share` is
/// **this client's** cap, which is the number a client should operate at.
fn too_many_in_flight(d: Share) -> ApiError {
    let why = if d.share < TILE_MAX_IN_FLIGHT || d.reserved > 0 {
        format!(
            " — {} client(s) are reading tiles, so your share is {} of them{}",
            d.clients,
            d.share,
            if d.reserved > 0 {
                ", and one slot is held for a client that has nothing on screen yet"
            } else {
                ""
            },
        )
    } else {
        String::new()
    };
    ApiError::new(
        503,
        format!(
            "too many tile reads in flight (limit {TILE_MAX_IN_FLIGHT}, share {}){why}: tile \
             production takes the history lock, so the cap is ingest backpressure, not a queue — \
             cancel tiles whose viewport you have left and retry the ones you still want",
            d.share
        ),
    )
}

/// Which open pyramid a tile address resolves against (T-439).
///
/// `scheme` was already *the lattice the address is expressed in*; with a view-scheme pyramid open
/// it is also **which store answers**, and the two cannot come apart — asking for the view lattice
/// and reading scheme 1's ladder is precisely the gap T-438 left: the coarse nodes had no store
/// behind them and fell back on a ladder whose time cell is a day.
///
/// The choice is one function so there is exactly one mapping, and so a server with no view
/// pyramid behaves as it did before (everything resolves against [`ApiState::history`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileStore {
    /// The de-welded view lattice ([`ApiState::view_history`]), whose finest node is the live edge.
    View,
    /// The spectrum-history pyramid `/api/history` answers from (scheme 1).
    Main,
}

/// Picks the store for this address. `scheme=view` (and the default) uses the view pyramid when
/// one is open; a numeric `scheme` uses whichever open pyramid *is* that scheme, so `scheme=2`
/// and `scheme=view` name the same store and cannot disagree.
pub fn tile_store(state: &ApiState, q: &Params) -> TileStore {
    let Some(v) = state.view_history.as_ref() else {
        return TileStore::Main;
    };
    match param(q, "scheme") {
        None | Some("view") => TileStore::View,
        // **The overview tier is the spectrum-history pyramid** (T-505). Its geometry does not
        // move when the view pyramid's floor does, which is the whole reason it can answer a
        // device-wide, record-long viewport at all — see [`TileLattice::overview`].
        Some("overview") => TileStore::Main,
        Some(other) => match other.parse::<u16>() {
            Ok(n) => {
                let is_view = v
                    .lock()
                    .map(|p| p.config().scheme == n)
                    .unwrap_or_else(|e| e.into_inner().config().scheme == n);
                if is_view {
                    TileStore::View
                } else {
                    TileStore::Main
                }
            }
            // Unparseable: `parse_key` refuses it, and the geometry it refuses against is
            // immaterial. Main keeps the pre-T-439 message.
            Err(_) => TileStore::Main,
        },
    }
}

/// Runs `f` on the pyramid `store` names.
pub(crate) fn with_tile_history<T>(
    state: &ApiState,
    store: TileStore,
    f: impl FnOnce(&hk_store::Pyramid) -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    match store {
        TileStore::Main => crate::http::with_history(state, f),
        TileStore::View => {
            let p = state
                .view_history
                .as_ref()
                .ok_or_else(|| ApiError::new(404, "no view-scheme history on this server"))?;
            let p = p
                .lock()
                .map_err(|_| ApiError::new(500, "view history store poisoned"))?;
            f(&p)
        }
    }
}

/// [`with_tile_history`], with the store **mutable first**, so a read can build the coarse node it
/// is asking for (`docs/16` §5.2 "on demand at the live edge", T-453) inside the same lock hold
/// that then reads it.
///
/// The build and the read cannot be split into two holds: a live-edge summary is dropped the
/// instant the frames under it change, so materialising under one lock and querying under the next
/// would race ingest and serve a tile that reads *unobserved* over data the store holds.
///
/// A store reached only through the floor product is immutable here and is scheme 1, whose coarse
/// levels are a seal-time product; [`hk_store::Pyramid::materialize`] is a no-op for it either way,
/// so that path simply reads.
pub(crate) fn with_tile_history_built<T>(
    state: &ApiState,
    store: TileStore,
    level: u8,
    freq: FreqRange,
    time: TimeRange,
    read: impl FnOnce(&hk_store::Pyramid) -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    let shared = match store {
        TileStore::View => state.view_history.as_ref(),
        TileStore::Main => state.history.as_ref(),
    };
    let Some(shared) = shared else {
        return with_tile_history(state, store, read);
    };
    let mut p = shared
        .lock()
        .map_err(|_| ApiError::new(500, "history store poisoned"))?;
    p.materialize(usize::from(level), freq, time).map_err(|e| {
        ApiError::new(
            400,
            format!("this tile's level cannot be built from the levels below it: {e}"),
        )
    })?;
    read(&p)
}

/// The lattice a tile address is expressed in.
///
/// Both forms give the same thing — a `(level_f, level_t)` grid of cell sizes — and differ only in
/// where the sizes come from, which is exactly the distinction that makes *"no such node"* a real
/// answer instead of a theoretical one.
#[derive(Clone, Debug, PartialEq)]
pub struct TileLattice {
    /// How the address was spelled: `"view"` or the store scheme's id.
    pub name: String,
    /// Frequency cell per `level_f`, finest first.
    pub f_cells_hz: Vec<f64>,
    /// Time cell per `level_t`, finest first, ns.
    pub t_cells_ns: Vec<i64>,
    /// `true` when the lattice is the store's own levels, so a missing node is a real refusal.
    pub from_store: bool,
}

impl TileLattice {
    /// The de-welded view lattice, anchored at the open pyramid's **own level-0 cell** (F1) and
    /// doubling each axis independently until §6.2's widest and tallest tile are reached.
    pub fn view(geom: &Geometry) -> Self {
        Self::doubling(geom, "view")
    }

    /// **The overview tier's lattice** (T-505): the same de-welded construction, anchored at the
    /// **spectrum-history** pyramid's level-0 cell and answered by that pyramid.
    ///
    /// # Why a second lattice, and why this is the only shape that works
    ///
    /// `axes.{frequency,time}.max_level` bounds level *indices*, never cell **size**, and
    /// [`readable_ceiling`] is blind to absolute size because [`servable`] reasons about ratios.
    /// So the coarsest tile a lattice can **address** shrinks by exactly the factor its floor
    /// shrank. Measured on real pyramids (`the_overview_tier_reaches_past_the_whole_surface`):
    ///
    /// | lattice's store | floor | ceiling | coarsest addressable tile |
    /// |---|---|---|---|
    /// | view pyramid, shipped floor | 6250 Hz × 1 s | (9, 1) | 819.2 MHz × 512 s |
    /// | view pyramid, T-484's floor | 585.9375 Hz × 40.1 ms | (9, 1) | **76.8 MHz × 20.5 s** |
    /// | **scheme 1, this lattice** | 6250 Hz × 1 s | **(11, 14)** | **3276.8 MHz × 48.5 days** |
    ///
    /// The middle row is T-484's dark map: the same ceiling index over a floor eight doublings
    /// finer is 133× less tile area, so a 6 GHz × 30 min minimap went from 32 addresses to 7031
    /// behind a four-slot in-flight cap and nothing arrived. **No lattice depth gives it back** —
    /// T-501 swept `f_levels × t_levels` in 2..=10 at both floors and the ceiling is identical
    /// index for index — because the real bound is *work*: a tile's source grid is
    /// `tile_hz / f_cell` by `tile_s / t_cell` over the store's **coarsest** level, so reach is
    /// proportional to that level's absolute cell size. A store whose finest cell is the display
    /// STFT's own bin has a coarsest cell that is finer in the same proportion, and it genuinely
    /// cannot back a device-wide tile at any price.
    ///
    /// So the fix is not a wider ceiling but a **second source whose cells are absolutely coarse**,
    /// and one is already open and already fed: `/api/history`'s scheme-1 pyramid, whose geometry
    /// does not move when the view pyramid's floor does. That is what makes the honesty tiers real
    /// in the tile *source* rather than only in the label (`docs/16` §8.5e, CLAUDE.md): the fine
    /// lattice answers the tuned window at live-IQ detail, this one answers wide-and-long
    /// viewports as spectrum-history / survey-overview, and [`tier`] still says per tile which of
    /// the two a cell came from — a tile folded from a coarser source cell is `survey-overview`
    /// here exactly as it is there.
    ///
    /// Grey does not move with the tier: the coverage plane is **record-derived**
    /// ([`crate::coverage::TileOverlay`]), not read out of whichever pyramid answered, so the two
    /// tiers cannot disagree about where the radio looked.
    pub fn overview(geom: &Geometry) -> Self {
        Self::doubling(geom, "overview")
    }

    fn doubling(geom: &Geometry, name: &str) -> Self {
        let f0 = geom.levels[0].f_cell_hz.max(f64::MIN_POSITIVE);
        let t0 = geom.levels[0].t_cell_ns.max(1);
        let cells = TILE_CELLS as f64;
        let mut f_cells_hz = vec![f0];
        while f_cells_hz.len() < MAX_VIEW_LEVELS
            && f_cells_hz.last().copied().unwrap_or(f0) * cells < VIEW_MAX_TILE_HZ
        {
            f_cells_hz.push(f_cells_hz.last().copied().unwrap_or(f0) * 2.0);
        }
        let mut t_cells_ns = vec![t0];
        while t_cells_ns.len() < MAX_VIEW_LEVELS
            && t_cells_ns
                .last()
                .copied()
                .unwrap_or(t0)
                .saturating_mul(TILE_CELLS as i64)
                < VIEW_MAX_TILE_NS
        {
            t_cells_ns.push(t_cells_ns.last().copied().unwrap_or(t0).saturating_mul(2));
        }
        Self {
            name: name.into(),
            f_cells_hz,
            t_cells_ns,
            from_store: false,
        }
    }

    /// The store's own lattice, read off the geometry by T-434's [`Geometry::f_axis`]/
    /// [`Geometry::t_axis`]. A welded ladder comes out as the **diagonal**, so most of this grid
    /// has no node — which is the honest statement of what a ladder is.
    pub fn store(geom: &Geometry, scheme: u16) -> Self {
        Self {
            name: scheme.to_string(),
            f_cells_hz: geom.f_axis(),
            t_cells_ns: geom.t_axis(),
            from_store: true,
        }
    }

    fn cell_of(&self, level_f: usize, level_t: usize) -> Option<(f64, i64)> {
        Some((
            *self.f_cells_hz.get(level_f)?,
            *self.t_cells_ns.get(level_t)?,
        ))
    }
}

/// A validated tile address, and the extent it names.
#[derive(Clone, Debug, PartialEq)]
pub struct TileKey {
    /// Whose coverage decides this tile's grey; `"any"` is the union and is never named.
    pub device: String,
    /// The lattice the address is expressed in.
    pub lattice: TileLattice,
    /// Frequency level: the `level_f` axis, 0 finest.
    pub level_f: usize,
    /// Time level: the `level_t` axis, 0 finest. **Independent of `level_f`.**
    pub level_t: usize,
    /// Tile index along frequency.
    pub f_index: i64,
    /// Tile index along time, from the Unix epoch.
    pub t_index: i64,
    /// Cells on each edge.
    pub cells: usize,
    /// This tile's frequency cell, Hz.
    pub f_cell_hz: f64,
    /// This tile's time cell, ns.
    pub t_cell_ns: i64,
    /// The tile's extent.
    pub region: Region,
    /// The store level whose cells are exactly this tile's, when one exists ([`Geometry::level_at`]).
    pub store_node: Option<usize>,
}

impl TileKey {
    fn window(&self) -> TimeRange {
        TimeRange::new(
            Timestamp::from_unix_nanos(self.region.t0_ns),
            Timestamp::from_unix_nanos(self.region.t1_ns),
        )
    }

    fn named_device(&self) -> bool {
        self.device != "any"
    }
}

fn integer(q: &Params, key: &'static str) -> Result<i64, ApiError> {
    param(q, key)
        .ok_or_else(|| bad(&format!("{key} is required")))?
        .parse::<i64>()
        .ok()
        .ok_or_else(|| bad(&format!("{key} must be an integer")))
}

/// Parses and validates a tile address against the open pyramid's geometry.
pub fn parse_key(geom: &Geometry, q: &Params) -> Result<TileKey, ApiError> {
    let cells = count(q, "cells", TILE_CELLS, TILE_CELLS)?;
    if cells < MIN_TILE_CELLS {
        return Err(bad(&format!(
            "cells must be in {MIN_TILE_CELLS}..={TILE_CELLS}"
        )));
    }
    let device = param(q, "device").unwrap_or("any").to_owned();
    if device.is_empty() || device.len() > 128 {
        return Err(bad("device must be `any` or a device id"));
    }
    let lattice = match param(q, "scheme") {
        None | Some("view") => TileLattice::view(geom),
        Some("overview") => TileLattice::overview(geom),
        Some(other) => {
            let n: u16 = other
                .parse()
                .map_err(|_| bad("scheme must be `view`, `overview` or a store scheme id"))?;
            TileLattice::store(geom, n)
        }
    };
    let level_f = integer(q, "level_f")?;
    let level_t = integer(q, "level_t")?;
    let (level_f, level_t) = match (usize::try_from(level_f), usize::try_from(level_t)) {
        (Ok(a), Ok(b)) => (a, b),
        _ => return Err(bad("level_f and level_t must be >= 0")),
    };
    // "No such node", spelled out: which axis ran out, and how far each one goes. A route that
    // silently clamped here would be the snap T-434 exists to refuse.
    let Some((f_cell_hz, t_cell_ns)) = lattice.cell_of(level_f, level_t) else {
        return Err(ApiError::new(
            404,
            format!(
                "scheme {:?} has no node at (level_f {level_f}, level_t {level_t}): its axes are \
                 level_f 0..{} and level_t 0..{}",
                lattice.name,
                lattice.f_cells_hz.len(),
                lattice.t_cells_ns.len()
            ),
        ));
    };
    // A welded ladder is the DIAGONAL of its own lattice (T-434): addressing it off-diagonal is a
    // node that does not exist, and the honest answer is to say so rather than serve a level whose
    // time cell is a day when a second was asked for.
    let store_node = node_of(geom, f_cell_hz, t_cell_ns);
    if lattice.from_store && store_node.is_none() {
        return Err(ApiError::new(
            404,
            format!(
                "scheme {:?} has no node at (level_f {level_f}, level_t {level_t}): this scheme is \
                 a welded ladder, so only the diagonal exists — its cells coarsen on both axes at \
                 once. Address the `view` lattice for independent axes.",
                lattice.name
            ),
        ));
    }
    let f_index = integer(q, "f_index")?;
    let t_index = integer(q, "t_index")?;
    if f_index < 0 || t_index < 0 {
        return Err(bad("f_index and t_index must be >= 0"));
    }
    let span_hz = f_cell_hz * cells as f64;
    let f_lo = f_index as f64 * span_hz;
    if !(f_lo.is_finite() && f_lo + span_hz <= 1e12) {
        return Err(bad(
            "f_index is outside the addressable spectrum (0 .. 1 THz)",
        ));
    }
    let span_ns = i128::from(t_cell_ns) * i128::from(cells as i64);
    let t0 = i128::from(t_index) * span_ns;
    if t0 + span_ns > i128::from(i64::MAX) / 2 {
        return Err(bad("t_index is outside the addressable time range"));
    }
    Ok(TileKey {
        device,
        lattice,
        level_f,
        level_t,
        f_index,
        t_index,
        cells,
        f_cell_hz,
        t_cell_ns,
        region: Region {
            freq: FreqRange::new(f_lo, f_lo + span_hz),
            t0_ns: t0 as i64,
            t1_ns: (t0 + span_ns) as i64,
        },
        store_node,
    })
}

/// The store level whose cells are **exactly** `(f_cell_hz, t_cell_ns)`, when the scheme has one.
///
/// A welded ladder is the DIAGONAL of its own lattice (T-434): addressing it off-diagonal is a node
/// that does not exist, and `None` here is what says so.
fn node_of(geom: &Geometry, f_cell_hz: f64, t_cell_ns: i64) -> Option<usize> {
    let lf = geom.f_axis().iter().position(|w| *w == f_cell_hz)?;
    let lt = geom.t_axis().iter().position(|d| *d == t_cell_ns)?;
    geom.level_at(lf, lt)
}

/// `(nt, nf)` of store level `l`'s own grid over `r`.
fn dims(geom: &Geometry, l: usize, r: &Region) -> (f64, f64) {
    let g = &geom.levels[l];
    let nf = ((r.freq.hi_hz / g.f_cell_hz).ceil() - (r.freq.lo_hz / g.f_cell_hz).floor()).max(1.0);
    let t0 = r.t0_ns.div_euclid(g.t_cell_ns);
    let t1 = (r.t1_ns.saturating_add(g.t_cell_ns - 1)).div_euclid(g.t_cell_ns);
    ((t1 - t0).max(1) as f64, nf)
}

/// The levels whose **read** over this tile fits the work budget — **one half of the question**,
/// and never an answer on its own. [`affordable_levels`] is the whole answer; this is private for
/// exactly that reason.
///
/// Ordered by **cell area**, explicitly, because T-434 proved index order is a coarseness order
/// only for a ladder: in a lattice node (1, 0) outranks (0, 3) in index while being finer in time.
/// Area is a total order that agrees with the partial coarsening order — if a level is no coarser
/// than another on both axes its area is no larger — so ordering by it never puts a coarser level
/// before a finer one.
///
/// The bound is a **work** bound, never a resolution budget: a level qualifies when its whole grid
/// over the tile fits [`TILE_MAX_TOTAL_SOURCE_CELLS`] and one chunk of it fits
/// [`TILE_MAX_SOURCE_CELLS`]. Because the preference is *finest*, the bound can only ever push the
/// answer toward a level at least as coarse as the finest one that fits — and that is stated as
/// replication, never hidden as grey.
fn read_affordable_levels(geom: &Geometry, key: &TileKey) -> Vec<usize> {
    let mut out: Vec<usize> = (0..geom.n_levels())
        .filter(|&l| {
            let (nt, nf) = dims(geom, l, &key.region);
            // One output row's worth of source, which is the smallest chunk a read can take.
            let rows_per_out =
                (key.t_cell_ns as f64 / geom.levels[l].t_cell_ns as f64).ceil() + 1.0;
            nt * nf <= TILE_MAX_TOTAL_SOURCE_CELLS as f64
                && rows_per_out * nf <= TILE_MAX_SOURCE_CELLS as f64
        })
        .collect();
    out.sort_by(|&a, &b| {
        let area = |l: usize| geom.levels[l].f_cell_hz * geom.levels[l].t_cell_ns as f64;
        area(a).total_cmp(&area(b)).then(a.cmp(&b))
    });
    out
}

/// Whether the **fold** this read would provoke at `level` fits `hk-store`'s materialize budget,
/// charged on an **empty** store — the worst store a read can meet, since a tile that already
/// exists costs no budget ([`hk_store::Pyramid::materialize_cost_bound`]).
///
/// The window is [`chunk_rows`]'s, not the tile's: the read is chunked into whole output rows and
/// `materialize` is called **per chunk** ([`with_tile_history_built`]), so the chunk is what the
/// budget is actually asked for. The last chunk can only be shorter, and the bound is monotone in
/// the window, so charging a full chunk is conservative for every chunk of the read.
fn fold_affordable(p: &hk_store::Pyramid, key: &TileKey, level: usize) -> bool {
    let window = chunk_rows(p.geometry(), key, level) as i64 * key.t_cell_ns;
    // Two things bound the blocks one chunk can touch, and a chunk fits the budget if EITHER does
    // (the true count is at most the smaller one):
    //
    // - its own start grid: `read_level` starts chunk k at `t0 + k * window`, and `t0` is a whole
    //   number of tile spans from the epoch, so every start is a multiple of gcd(window, span);
    // - the tile it lies inside: a chunk never leaves its tile, so it cannot touch a block the
    //   whole tile does not. The tile starts on its own span, and at `(9, 1)` on the shipped
    //   store that span is one 512 s block. The window grid alone says a 30 s chunk may straddle
    //   two blocks. The tile says there is only one to straddle.
    //
    // The last chunk is shorter, starts on the same grid and lies in the same tile. The bound is
    // monotone in the window, so that chunk is covered too.
    let span = key.t_cell_ns.saturating_mul(key.cells as i64);
    let freq = key.region.freq;
    p.materialize_cost_bound(level, freq, window, gcd_ns(window, span))
        .or_else(|| p.materialize_cost_bound(level, freq, span, span))
        .is_some()
}

/// Greatest common divisor of two durations; `0` only when both are `0`.
fn gcd_ns(mut a: i64, mut b: i64) -> i64 {
    while b != 0 {
        (a, b) = (b, a.rem_euclid(b));
    }
    a.abs()
}

/// The store levels that may back this tile, **finest first** — affordable to **read** *and*
/// affordable to **build**.
///
/// # Why both halves are one predicate (T-494)
///
/// They were two, and they disagreed. This function used to answer only the read half, and
/// `tile_read` then walked its answer straight into `hk-store`'s `materialize`, which refuses a
/// fold past [`hk_store::history::MAX_MATERIALIZE_TILES`]. A level can be cheap to read and
/// impossible to build — a coarse node's grid over a tile is small *because* the node is coarse,
/// and the same coarseness is what makes folding it from nothing expensive — so the two predicates
/// contradicted each other on a large part of every lattice, measured: on the shipped 4 × 4 store
/// every address with `level_t >= 5` listed the frequency-coarsest column as a candidate and none
/// of that column could be folded.
///
/// [`servable`] papered over it the only way a per-address `bool` can: by demanding **every**
/// candidate be buildable, so one unbuildable level poisoned the address even though `tile_read`
/// would only ever reach it after everything finer held nothing. That is what capped the ladder —
/// on a 6 × 6 store the ceiling fell to `(7, 2)` and on 8 × 8 to `(0, 0)`, because a deeper store
/// has *more* coarse nodes to be poisoned by.
///
/// **The fix is not to loosen `materialize`.** Its refusal is real: the fold it declines genuinely
/// costs more than 1024 tiles. The fix is that a level nobody can build is not a candidate, so the
/// walk never offers it and `servable` reduces to *"is any level left?"*. Skipping it can only
/// serve more than refusing the whole address did, because the walk continues to the coarser levels
/// that follow it.
///
/// The answer does **not** depend on what the store holds — `materialize_cost_bound` is taken on an
/// empty store — so this stays a pure function of geometry and config, which is what lets
/// [`readable_ceiling`] quote it in a contract.
pub fn affordable_levels(p: &hk_store::Pyramid, key: &TileKey) -> Vec<usize> {
    let mut out = read_affordable_levels(p.geometry(), key);
    out.retain(|&l| fold_affordable(p, key, l));
    out
}

/// Output rows one chunk of a read at `level` covers — **one history lock hold each**.
///
/// Shared by the read and by [`servable`] on purpose: the readable ceiling is a claim about what
/// the read will do, so the two must not be able to disagree about how the read is cut up.
pub(crate) fn chunk_rows(geom: &Geometry, key: &TileKey, level: usize) -> usize {
    let g = &geom.levels[level];
    let (_, nf) = dims(geom, level, &key.region);
    let rows_per_out = (key.t_cell_ns as f64 / g.t_cell_ns as f64).ceil() + 1.0;
    let per_out_row = (rows_per_out * nf).max(1.0);
    let rows = (TILE_MAX_SOURCE_CELLS as f64 / per_out_row)
        .floor()
        .max(1.0);
    (rows as usize).min(key.cells)
}

/// Can [`tile_read`] answer this address **at all**, on the worst store it could meet?
///
/// Both of the route's two refusals, asked before either is provoked — and asked as **one**
/// question, because [`affordable_levels`] now carries both (T-494): a level survives it only if
/// its read fits the work budget *and* its fold fits `hk-store`'s materialize budget on an empty
/// store. So servability is exactly *"is any level left?"*.
///
/// **This is the whole candidate list, not merely the finest.** `tile_read` walks the candidates in
/// order and stops at the first that *holds something*, so on a store where the fine levels are
/// empty — which is every store, over any band it has not tuned — the walk reaches the coarse ones.
/// Every level it can reach has to be answerable, and it is, because every level it can reach is in
/// this list.
pub fn servable(p: &hk_store::Pyramid, key: &TileKey) -> bool {
    !affordable_levels(p, key).is_empty()
}

/// The address at `(level_f, level_t)` on this lattice, at index `(0, 0)`.
///
/// Index is immaterial to servability on this route's own lattices and it is worth saying why
/// rather than assuming it: a tile's extent is a power-of-two multiple of the store's own cell on
/// both axes, and so is a store block, so a tile either contains whole blocks or lies inside one
/// **at every index**. The one offset-dependent quantity — where a read's *time chunks* fall
/// against the blocks — is not indexed at all; `materialize_cost_bound` charges it the worst
/// alignment by construction.
fn probe_key(
    lattice: &TileLattice,
    cells: usize,
    level_f: usize,
    level_t: usize,
) -> Option<TileKey> {
    let (f_cell_hz, t_cell_ns) = lattice.cell_of(level_f, level_t)?;
    Some(TileKey {
        device: "any".into(),
        lattice: lattice.clone(),
        level_f,
        level_t,
        f_index: 0,
        t_index: 0,
        cells,
        f_cell_hz,
        t_cell_ns,
        region: Region {
            freq: FreqRange::new(0.0, f_cell_hz * cells as f64),
            t0_ns: 0,
            t1_ns: t_cell_ns.saturating_mul(cells as i64),
        },
        store_node: None,
    })
}

/// **How far up each axis this route can actually be READ** (T-482).
///
/// # The defect this exists for
///
/// The view lattice is 12 × 15 on the shipped geometry and the store behind it is 4 × 4, so the
/// coarse corner is **named but unbackable**: the route declared `levels` and then `400`d a large
/// part of the grid it had just declared. Measured against a real `hk serve`, an aggressive
/// zoom-out made 117 tile requests of which 107 were refused — and the client was not misbehaving,
/// it was obeying the only bound anyone stated. *Nothing said is never permissive*, inverted: the
/// route promised more than it could deliver, and every client that believed it was punished.
///
/// # What the number means, and what a per-axis pair cannot say
///
/// `(max_f, max_t)` is a **box**: every address with `level_f <= max_f` and `level_t <= max_t` is
/// servable. That is the strong reading, and it is the one a client can use, because the client
/// clamps each axis on its own.
///
/// **The servable set is not a box, though, and a box therefore loses some of it.** The binding
/// constraint at the coarse end is the *work* budget, and work goes as the tile's **area** — a
/// store level's grid over the tile is `tile_hz / f_cell` by `tile_s / t_cell`, so the budget reads
/// `level_f + level_t <= k`, an anti-diagonal. A box inscribed in an anti-diagonal cannot reach
/// both of its far corners: on the shipped geometry `(9, 1)` and `(5, 5)` are both servable and
/// both maximal, and no single box holds them both. What is lost is the off-corner pairs — a very
/// wide tile *and* a very tall one, which is exactly the combination whose work bound is the reason
/// for the ceiling in the first place.
///
/// The tie is settled toward **frequency**, and measured rather than argued: covering the canvas's
/// own widest view (1 MHz–6 GHz by a retention window of tens of minutes) costs 32 tiles at
/// `(9, 1)`, 30 at `(8, 2)` and `(7, 3)`, and 118 at `(5, 5)` — so the sum is what matters and the
/// tie-break barely does, and it goes to the axis the defect appeared on: the frequency surface is
/// 6 GHz wide and always at full extent, while the time surface is a retention window that usually
/// fits inside one tile whatever the level.
///
/// **But not past zero on an axis** (T-571). Before the coarse nodes were maintained live the fold
/// budget capped frequency, so the tie never reached the end of its anti-diagonal; with the fold
/// gone it does, and the frequency preference alone picked `(10, 0)` over `(9, 1)` on the shipped
/// geometry — identical area, identical 32 tiles for the widest view, and a `max_level` of **0**
/// on the time axis. A zero there is not "one level less reach": `ui/src/surface/lattice.ts`
/// skips any ancestor whose `levelT` exceeds the cap, so a cap of 0 emits **no time-coarser
/// ancestor at all** and `docs/16` §5.5's draw-a-coarser-resident-tile-while-the-fine-one-loads
/// path loses its time arm — a time zoom-out draws *not loaded* instead of a coarse stand-in. It
/// also makes the tile count on that axis grow linearly with the span, and a pane over a tuned
/// 20 MHz band is already narrower than one tile at `level_f = 9`, so the frequency level it was
/// traded for buys that pane nothing. So a pair with both axes ≥ 1 wins an equal-area tie, and
/// frequency decides only among those.
///
/// # Readability, not existence
///
/// A node a scheme does not *have* — a welded ladder's off-diagonal, which `parse_key` answers with
/// its own 404 and `axes.store_node` already reports — is skipped here rather than counted against
/// the ceiling. The two are different claims and conflating them would collapse a ladder's ceiling
/// to `(0, 0)` while saying nothing new.
///
/// # It is stated at [`TILE_CELLS`], NOT at this answer's `cells`
///
/// The bound is on tile **area**, so it does move with `cells` — and quoting it *per answer* would
/// be the adjacent question rather than the one a client asks. `ui/src/surface/tile.ts` bootstraps
/// its lattice from a deliberately cheap **`cells = 8`** probe and then renders the surface at 256
/// (`latticeOf(probe, RENDER_CELLS)`), so a ceiling quoted for the probe's own tile size would be
/// cached against tiles 32× wider on each axis and would be a lie for every one of them — measured
/// in the browser tier, where a per-answer ceiling read back as `(11, 9)` from the probe while the
/// page drew at 256 and 69 addresses inside that box were refused.
///
/// So the ceiling is a property of the **surface**, not of the probe that fetched it, and 256 is
/// the canvas's own tile unit (`docs/16` §6.2). It is also the **most conservative** statement over
/// the whole `cells` range this route accepts, since a bigger tile is strictly harder to back — so
/// it cannot over-claim for a caller using any smaller tile either. What it costs is reach for such
/// a caller: at `cells = 32` more levels are genuinely readable than this pair names.
pub fn readable_ceiling(p: &hk_store::Pyramid, lattice: &TileLattice) -> (usize, usize) {
    let cells = TILE_CELLS;
    let geom = p.geometry();
    let nf = lattice.f_cells_hz.len();
    let nt = lattice.t_cells_ns.len();
    // `reach[lt]`: the largest `level_f` with every `(0..=level_f, lt)` servable, or `None` when
    // even `(0, lt)` is not.
    let reach = |lt: usize| -> Option<usize> {
        let mut last = None;
        for lf in 0..nf {
            let Some(key) = probe_key(lattice, cells, lf, lt) else {
                break;
            };
            // A node this scheme does not have is not an unreadable node (see above).
            let exists =
                !lattice.from_store || node_of(geom, key.f_cell_hz, key.t_cell_ns).is_some();
            if exists && !servable(p, &key) {
                break;
            }
            last = Some(lf);
        }
        last
    };
    let (mut best, mut floor_f) = ((0usize, 0usize), usize::MAX);
    // Among equal-area maxima, a pair that keeps BOTH axes alive beats one that does not; only
    // then does frequency decide. See the doc comment: a `0` on an axis is not one level less
    // reach, it deletes that axis's ancestor ladder in the client.
    let rank = |(f, t): (usize, usize)| (f + t, usize::from(f >= 1 && t >= 1), f);
    for lt in 0..nt {
        let Some(r) = reach(lt) else { break };
        floor_f = floor_f.min(r);
        if rank((floor_f, lt)) > rank(best) {
            best = (floor_f, lt);
        }
    }
    best
}

/// **[`readable_ceiling`], memoised per lattice** (T-579).
///
/// The ceiling probes the whole lattice — `nf × nt` addresses, each through [`servable`] and
/// `materialize_cost_bound` — and `GET /api/tiles` states it on every answer, so before this it was
/// recomputed from scratch on every one of a screen's 135–290 tile requests. It is a **pure
/// function** of the lattice, the store's geometry and its `coarse_on_demand` switch (the only
/// config `materialize_cost_bound` reads) — nothing the store *holds* enters it, which is the
/// whole point of the ceiling (a client caches it). So the key is exactly those three, spelled in
/// full, and an entry never needs invalidating: a different geometry is a different key.
#[derive(Default)]
pub struct CeilingMemo {
    inner: Mutex<CeilingMemoInner>,
}

#[derive(Default)]
struct CeilingMemoInner {
    map: std::collections::HashMap<String, (usize, usize)>,
    computed: u64,
}

/// Distinct (lattice, geometry) pairs held; a server has two or three. Past this the table is
/// dropped whole rather than grown, which only costs a recomputation.
const CEILING_MEMO_MAX: usize = 64;

impl CeilingMemo {
    /// [`readable_ceiling`] for `lattice` over `p`, computed at most once per distinct key.
    pub fn ceiling(&self, p: &hk_store::Pyramid, lattice: &TileLattice) -> (usize, usize) {
        let key = format!(
            "{lattice:?}|{:?}|{}",
            p.geometry(),
            p.config().coarse_on_demand
        );
        if let Some(c) = self
            .inner
            .lock()
            .ok()
            .and_then(|g| g.map.get(&key).copied())
        {
            return c;
        }
        let c = readable_ceiling(p, lattice);
        if let Ok(mut g) = self.inner.lock() {
            g.computed += 1;
            if g.map.len() >= CEILING_MEMO_MAX {
                g.map.clear();
            }
            g.map.insert(key, c);
        }
        c
    }

    /// Lattice probes performed — full [`readable_ceiling`] computations — since this state was
    /// built.
    pub fn computations(&self) -> u64 {
        self.inner.lock().map_or(0, |g| g.computed)
    }
}

/// One tile's measurement plane, and what produced it.
pub struct TileRead {
    /// The tile's grid, exactly `cells × cells` on the tile's own extent.
    pub grid: Overview,
    /// The store level that **answered** — not the one implied by the address (T-426's rule).
    pub level: u8,
    /// That level's cells.
    pub src_f_cell_hz: f64,
    /// That level's time cell, ns.
    pub src_t_cell_ns: i64,
    /// Every level consulted, in order. More than one means a finer level held nothing and the
    /// read walked coarser.
    pub tried: Vec<u8>,
    /// Affordable levels, finest first.
    pub candidates: Vec<u8>,
    /// Source cells actually read.
    pub source_cells: usize,
    /// Chunks the read was split into — one history lock hold each.
    pub chunks: usize,
}

/// Reads one tile, chunked by whole output rows so no single history lock hold exceeds
/// [`TILE_MAX_SOURCE_CELLS`].
fn read_level(
    state: &ApiState,
    store: TileStore,
    key: &TileKey,
    level: u8,
) -> Result<TileRead, ApiError> {
    let cells = key.cells;
    let (rows_per_chunk, nf_src, level_f_cell, level_t_cell) =
        with_tile_history(state, store, |p| {
            let g = &p.geometry().levels[level as usize];
            let (_, nf) = dims(p.geometry(), level as usize, &key.region);
            Ok((
                chunk_rows(p.geometry(), key, level as usize),
                nf as usize,
                g.f_cell_hz,
                g.t_cell_ns,
            ))
        })?;
    let mut out = vec![OverviewCell::UNOBSERVED; cells * cells];
    let mut unit = None;
    let (mut src_nt, mut source_cells, mut chunks) = (0usize, 0usize, 0usize);
    let t0 = key.region.t0_ns;
    let mut row = 0usize;
    while row < cells {
        let hi = (row + rows_per_chunk).min(cells);
        let chunk = TimeRange::new(
            Timestamp::from_unix_nanos(t0 + row as i64 * key.t_cell_ns),
            Timestamp::from_unix_nanos(t0 + hi as i64 * key.t_cell_ns),
        );
        // One lock hold per chunk: the history mutex is released between chunks so a tile fan-out
        // at the live edge never holds it for a whole tile (docs/16 §5.5 cap 3). T-453 builds the
        // coarse node inside that same hold, per chunk, so the chunking that bounds the read bounds
        // the build too — and so an ingest between chunks re-builds the live edge rather than
        // leaving half the tile unobserved.
        let part = with_tile_history_built(state, store, level, key.region.freq, chunk, |p| {
            let h = p
                .query(&RegionQuery {
                    freq: key.region.freq,
                    time: chunk,
                    resolution: Resolution::Level(level),
                })
                .map_err(|_| ApiError::new(400, "tile query refused"))?;
            Ok(h.overview(chunk, key.region.freq, hi - row, cells))
        })?;
        unit.get_or_insert(part.unit);
        src_nt += part.src_nt;
        source_cells += part.src_nt * part.src_nf;
        chunks += 1;
        out[row * cells..hi * cells].copy_from_slice(&part.cells);
        row = hi;
    }
    let (mut lo_db, mut hi_db) = (f32::INFINITY, f32::NEG_INFINITY);
    let mut observed_cells = 0;
    for c in &out {
        if c.sources == 0 {
            continue;
        }
        observed_cells += 1;
        lo_db = lo_db.min(c.max_db);
        hi_db = hi_db.max(c.max_db);
    }
    Ok(TileRead {
        grid: Overview {
            unit: unit.unwrap_or(hk_model::PowerUnit::Dbfs),
            nt: cells,
            nf: cells,
            t0_ns: key.region.t0_ns,
            t_cell_ns: key.t_cell_ns as f64,
            f_lo_hz: key.region.freq.lo_hz,
            f_cell_hz: key.f_cell_hz,
            cells: out,
            observed_cells,
            src_nt,
            src_nf: nf_src,
            range_db: (observed_cells > 0).then_some((lo_db, hi_db)),
        },
        level,
        src_f_cell_hz: level_f_cell,
        src_t_cell_ns: level_t_cell,
        tried: vec![level],
        candidates: Vec::new(),
        source_cells,
        chunks,
    })
}

/// Reads one tile: the finest affordable level, walking **coarser** only when a level holds nothing.
pub fn tile_read(state: &ApiState, store: TileStore, key: &TileKey) -> Result<TileRead, ApiError> {
    let (candidates, read_only) = with_tile_history(state, store, |p| {
        let read = read_affordable_levels(p.geometry(), key);
        let both: Vec<usize> = read
            .iter()
            .copied()
            .filter(|&l| fold_affordable(p, key, l))
            .collect();
        Ok((both, read))
    })?;
    if candidates.is_empty() {
        // Which half emptied it is the difference between "this tile is too wide for the history's
        // coarsest cell" and "the levels that would fit cannot be folded from nothing", and a
        // caller can act on one and not the other. Saying "no level fits" for both would be the
        // same conflation T-494 removed from the predicate.
        return Err(ApiError::new(
            400,
            if read_only.is_empty() {
                "no store level can back this tile inside the work budget: the spectrum history's \
                 coarsest cells are still too fine for a tile this wide. This is the view \
                 lattice's own scheme being absent, not an unobserved region."
                    .to_string()
            } else {
                format!(
                    "no store level can back this tile: {} level(s) are cheap enough to READ over \
                     this extent, and none of them can be FOLDED inside the store's materialize \
                     budget from nothing. Ask for a finer level, or a smaller region — \
                     `axes.{{frequency,time}}.max_level` states how far up this route can be read.",
                    read_only.len()
                )
            },
        ));
    }
    let listed: Vec<u8> = candidates.iter().map(|&l| l as u8).collect();
    let mut tried = Vec::new();
    let mut first: Option<TileRead> = None;
    for &level in &listed {
        let mut got = read_level(state, store, key, level)?;
        tried.push(level);
        if got.grid.observed_cells > 0 {
            got.tried = tried;
            got.candidates = listed;
            return Ok(got);
        }
        first.get_or_insert(got);
    }
    let mut r = first.expect("at least one candidate was read");
    r.tried = tried;
    r.candidates = listed;
    Ok(r)
}

/// The honesty tier for one tile read.
///
/// Two independent downgrades, and the weaker claim always wins: a tile wider than any capture
/// window is `survey-overview` by T-341's rule, and so is a tile whose source cells are **coarser**
/// than its own on either axis, because then a measured value is repeating across output cells
/// rather than each one carrying its own measurement.
fn tier(key: &TileKey, r: &TileRead, max_live_span_hz: Option<f64>) -> DetailSource {
    let replicated = r.src_f_cell_hz > key.f_cell_hz * 1.000_001
        || r.src_t_cell_ns as f64 > key.t_cell_ns as f64 * 1.000_001;
    if replicated {
        return DetailSource::SurveyOverview;
    }
    base_tier(key, max_live_span_hz)
}

/// The tier before any replication claim: what a tile of this **width** is, whatever answered it.
///
/// Never `live-iq` — and since **T-484 that is an under-claim, kept on purpose**.
///
/// T-439 made the view lattice's finest node the growing edge ("live" is a viewport, not a mode,
/// docs/16 §8.1), and the reasoning that stood here said a tile is still a *pyramid* read served at
/// a level's cell size, so a 6.25 kHz × 1 s cell is not "live IQ from the front end at the
/// resolution shown" however recently it was written. **That premise has gone:** T-484 sets node
/// (0, 0)'s cell to the display STFT's own bin and row and folds the published rows into it 1:1, so
/// such a tile now *is* the front end at the resolution shown, and `LiveIq` would be true.
///
/// It still answers `SpectrumHistory`, because [`crate::navigation::live_window_verdict`] defines
/// the enum as a claim about **span** — could this width have come from one capture window — and
/// four routes share that definition. Re-pointing it at *resolution* for one of them is a contract
/// change nothing has asked for, and docs/api.md's own rule settles the direction: under-claiming
/// costs a styling cue, over-claiming is the lie the invariant forbids. The distinction a reader
/// actually needs is served per tile and is now sharper than the enum: `resolution.answered.level`
/// `0` with `resolution.fold.*.direction` `exact` on both axes means one published row per cell,
/// and anything coarser is a declared fold of those.
fn base_tier(key: &TileKey, max_live_span_hz: Option<f64>) -> DetailSource {
    match crate::navigation::live_window_verdict(key.region.freq.width_hz(), max_live_span_hz) {
        DetailSource::LiveIq => DetailSource::SpectrumHistory,
        other => other,
    }
}

/// Whether the coverage map answered this tile on its own, and — when it did not — **why not**
/// (T-461).
///
/// Served on *both* paths, so "did the short-circuit fire?" is a question the wire answers rather
/// than one a test has to infer from a timing. `applied` is true **if and only if**
/// `selected_plane_uniform` is `"unobserved"`: a mixed plane, or a uniformly `"observed"` or
/// `"unknown"` one, takes the full read.
fn short_circuit_json(uniform: Option<&'static str>) -> Value {
    json!({
        "applied": uniform == Some("unobserved"),
        "selected_plane_uniform": uniform,
        "rule": "if and only if the SELECTED coverage plane — the one `coverage.selected.plane` \
            names, which is the same plane this answer serves and the same one the renderer greys \
            from — is \"unobserved\" for EVERY cell of this tile's extent, the tile is answered \
            from it: no pyramid query, no level walk, no grid enumeration, and `grid` states its \
            one cell instead of enumerating cells x cells of it. It FAILS CLOSED in every other \
            case: one observed cell, or one \"unknown\" cell (T-423 — we no longer know whether we \
            looked, which is not \"nothing looked\"), and the full read runs. The coverage plane is \
            read at this tile's own cells or coarser, and a coarser cell is unobserved only when no \
            tune span touches it at all, so uniform-unobserved coarser implies uniform-unobserved \
            finer — the predicate cannot be weakened by the grid it was evaluated on.",
    })
}

/// The answer for a tile whose selected coverage plane is `unobserved` end to end (T-461).
///
/// **Measured:** the read this replaces cost 92 ms of the route's own `cost.build_ms` and 2.56 MB
/// on the wire to say *nothing here*, with `source_cells: 65 536` — the full grid walked because
/// the cost is `O(cells²)` and essentially independent of whether any data exists. And the empty
/// case is the **common** case: T-437 measured the default full-device view at 99.4 % grey before
/// history accumulates, settling to 55.2 %.
///
/// It is not a second answer, it is a cheaper spelling of the same one. The client's own rule is
/// *"a cell the coverage plane calls unobserved stays unobserved even with a level beside it"*
/// (`ui/src/surface/tile.ts`), so the measurement the full read would have produced is discarded by
/// the renderer cell for cell — which is why this is observationally equivalent and not merely
/// faster.
/// This read's own diagnostics, grouped: `build_ms` and `in_flight` describe the *request*, not
/// the tile. T-574 had to strip exactly these two before hashing a response into an ETag, because
/// two reads of an unchanged sealed tile otherwise differ — so they are one concept, and passing
/// them as one argument says so (and keeps this helper inside clippy's argument budget).
struct ReadDiagnostics<'a> {
    elapsed_ms: f64,
    slot: &'a TileSlot,
}

// Eight even with the diagnostics grouped: the six the answer is assembled from, the sealed flag
// T-574 reads from the tile's own time extent, and (T-533) the plane spelling the caller asked
// for. It is one serialiser of one answer, called once.
#[allow(clippy::too_many_arguments)]
fn unobserved_tile_json(
    key: &TileKey,
    store: TileStore,
    ceiling: (usize, usize),
    coverage: Value,
    max_live: Option<f64>,
    diags: ReadDiagnostics<'_>,
    sealed: bool,
    planes: Planes,
) -> Value {
    let ReadDiagnostics { elapsed_ms, slot } = diags;
    let source = base_tier(key, max_live);
    json!({
        "sealed": sealed,
        "key": key_json(key),
        "extent": {
            "f_lo_hz": key.region.freq.lo_hz,
            "f_hi_hz": key.region.freq.hi_hz,
            "f_cell_hz": key.f_cell_hz,
            "t0_s": key.region.t0_ns as f64 / 1e9,
            "t1_s": key.region.t1_ns as f64 / 1e9,
            "t_cell_s": key.t_cell_ns as f64 / 1e9,
            "nt": key.cells,
            "nf": key.cells,
        },
        "axes": axes_json(key, ceiling),
        "grid": unobserved_grid_json(key, planes),
        "coverage": coverage,
        "resolution": {
            "source": source.as_str(),
            "live": source.is_live(),
            "statement": source.statement(),
            // NOTHING answered, because nothing was asked. `null` rather than a level, because
            // naming a level here would claim a read that did not happen — the same direction as
            // `tried: []`, which says the walk did not occur rather than that it found nothing.
            "answered": Value::Null,
            "candidates": Value::Array(Vec::new()),
            "tried": Value::Array(Vec::new()),
            "store": match store {
                TileStore::View => "view-lattice",
                TileStore::Main => "spectrum-history",
            },
            "fold": {
                // No source cells were read, so there is no fold and in particular no REPLICATION:
                // a replicated axis is the only direction that makes a claim, and nothing here
                // claims anything.
                "frequency": axis_fold(key.f_cell_hz, key.f_cell_hz, 0, key.cells),
                "time": axis_fold(key.t_cell_ns as f64, key.t_cell_ns as f64, 0, key.cells),
                "rule": "no level was folded onto this tile because none was consulted: the \
                    coverage map answered it (see `short_circuit`). `source_cells: 0` is the \
                    honest count of what was read.",
            },
            "budget": {
                "max_source_cells_per_lock": TILE_MAX_SOURCE_CELLS,
                "max_source_cells_per_tile": TILE_MAX_TOTAL_SOURCE_CELLS,
                "statement": "these bound WORK, never resolution — and no work was done here: the \
                    coverage map decides grey, and it greyed the whole tile.",
            },
            "short_circuit": short_circuit_json(Some("unobserved")),
            "grey_rule": "grey is decided by `coverage`, never by this block: a level that holds \
                nothing here is a level, and a cell nothing ever sampled is grey. Not-loaded is a \
                third thing and is the client's to draw (docs/16 §5.5).",
        },
        "cost": {
            "build_ms": (elapsed_ms * 1000.0).round() / 1000.0,
            // Zero, and that is the whole ticket: the read this replaces walked 65 536 source cells
            // to produce 65 536 nulls over spectrum no record says was ever sampled.
            "source_cells": 0,
            "chunks": 0,
            "in_flight": slot.in_flight(),
            "in_flight_limit": TILE_MAX_IN_FLIGHT,
            // T-630: the cap is server-wide, the SHARE is this client's, and the share is the
            // number a client should operate at. `clients` is how many are reading tiles right
            // now, so a client can see why its share moved.
            "in_flight_share": slot.share().share,
            "in_flight_held": slot.held(),
            "clients": slot.share().clients,
            "client": slot.client(),
            "reserved": slot.share().reserved,
            "fair_share": slot.share().fair,
            "statement": "this tile's grid was answered from the coverage map alone (T-461): no \
                history lock was taken for it, so `chunks` is 0. The last-known search behind \
                `shadow` (T-519) is separate and states its own holds in `shadow.search.chunks`; \
                over spectrum no tile holds it answers from the pyramid's tile index without \
                reading a cell.",
        },
    })
}

fn axis_fold(source_cell: f64, tile_cell: f64, source_cells: usize, served: usize) -> Value {
    let direction = if source_cell > tile_cell * 1.000_001 {
        "replicated"
    } else if source_cell < tile_cell * 0.999_999 {
        "folded"
    } else {
        "exact"
    };
    json!({
        "source_cell": source_cell,
        "tile_cell": tile_cell,
        "source_cells": source_cells,
        "served": served,
        "direction": direction,
        "replicated": direction == "replicated",
    })
}

/// How the `max_db` plane is spelled on the wire (T-533, `?planes=`).
///
/// **Not a compression setting: a choice of representation, named on the wire.** The value plane's
/// destination is an R16F texture, so [`Planes::F16`] sends exactly the bits that survive — and
/// `grid.encoding.planes` says which spelling was used, per answer, so a reader never has to infer
/// it from whether a field happens to be present. A client that does not recognise the name served
/// must refuse the tile rather than render it (`ui/src/surface/tile.ts` throws `TileDecodeError`);
/// a future packing gets a **new name**, never a redefinition of this one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Planes {
    /// One JSON number (or `null`) per cell. The default, and what every non-canvas reader of this
    /// route still gets.
    Json,
    /// `max_db` as base64 of little-endian IEEE binary16; the other three planes unchanged.
    ///
    /// Only `max_db`, because only `max_db` wins: on a full 256 × 256 live tile its JSON text is
    /// 1 197 118 B against 174 764 B packed, while `frames` is 131 073 B as text against 349 528 B
    /// as base64 `u32`, and `occupancy_max`/`coverage` lose by a similar factor. Packing them too
    /// would grow the body by 394 kB to save nothing.
    F16,
}

impl Planes {
    fn as_str(self) -> &'static str {
        match self {
            Planes::Json => "json",
            Planes::F16 => "f16",
        }
    }
}

/// `?planes=`: `json` (the default) or `f16`. An unrecognised spelling is a **400 naming it**,
/// never a silent fall back to JSON — a client that asked for a representation it can decode and
/// was quietly given another one would mis-read every cell.
fn parse_planes(q: &Params) -> Result<Planes, ApiError> {
    match q
        .iter()
        .find(|(k, _)| k == "planes")
        .map(|(_, v)| v.as_str())
    {
        None | Some("json") => Ok(Planes::Json),
        Some("f16") => Ok(Planes::F16),
        Some(other) => Err(bad(&format!(
            "planes={other:?} is not a plane encoding this server serves (json, f16)"
        ))),
    }
}

/// `f32` → IEEE 754 binary16 bits, round-to-nearest-even.
///
/// **NaN stays NaN** — on this wire NaN is *not observed*, exactly as `null` is in the JSON
/// spelling, so a conversion that turned it into an infinity or a zero would invent a measurement.
/// A magnitude past binary16's range becomes an infinity, which the client reads as non-finite and
/// therefore as absent too; no finite dB level this route serves is anywhere near 65 504.
fn f16_bits(x: f32) -> u16 {
    let b = x.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let raw_exp = (b >> 23) & 0xff;
    let mant = b & 0x007f_ffff;
    if raw_exp == 0xff {
        // Infinity, or a NaN kept a NaN (a non-zero payload bit set so it cannot become infinity).
        return sign | 0x7c00 | if mant != 0 { 0x0200 } else { 0 };
    }
    let exp = raw_exp as i32 - 127 + 15;
    if exp >= 31 {
        return sign | 0x7c00;
    }
    if exp <= 0 {
        if exp < -10 {
            return sign; // below the smallest subnormal: zero, with its sign
        }
        // Subnormal binary16: shift the implicit leading 1 back in, then round to nearest even.
        let m = mant | 0x0080_0000;
        let shift = (14 - exp) as u32; // 14..=24
        let keep = m >> shift;
        let round_bit = (m >> (shift - 1)) & 1;
        let sticky = (m & ((1 << (shift - 1)) - 1)) != 0;
        let inc = u32::from(round_bit == 1 && (sticky || (keep & 1) == 1));
        return sign | (keep + inc) as u16;
    }
    let keep = mant >> 13;
    let round_bit = (mant >> 12) & 1;
    let sticky = (mant & 0x0fff) != 0;
    let inc = u32::from(round_bit == 1 && (sticky || (keep & 1) == 1));
    // A mantissa that rounds up to 0x400 carries into the exponent by construction, which is what
    // `+` does here; at exp 30 that carries to 31 and the value becomes an infinity, correctly.
    sign | (((exp as u32) << 10) + keep + inc) as u16
}

/// One plane, packed: base64 of the little-endian binary16 values, with its own type stated.
fn f16_plane(values: impl Iterator<Item = f32>, cells: usize, scale: &str) -> Value {
    let mut bytes = Vec::with_capacity(cells * 2);
    for v in values {
        bytes.extend_from_slice(&f16_bits(v).to_le_bytes());
    }
    json!({
        "type": "f16",
        "byte_order": "little-endian",
        "transfer": "base64",
        "cells": cells,
        "bytes": bytes.len(),
        "scale": scale,
        // The same claim `null` makes in the JSON spelling, in the only encoding binary16 has for
        // it. **Never a zero and never a floor**: C26's rule is unchanged by the representation.
        "absent": "nan",
        "data": base64(&bytes),
    })
}

/// Standard base64, no line breaks — the alphabet `atob` reads.
fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            A[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            A[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// What the answer says about how its per-cell planes are spelled — present in **every** answer,
/// including the JSON one and the uniform short-circuit, so a reader never infers the encoding from
/// which fields happen to be present.
fn encoding_json(planes: Planes) -> Value {
    json!({
        "planes": planes.as_str(),
        "order": "row-major: time then frequency, earliest row and lowest frequency first — the \
            same cell order in either spelling",
        "rule": "`planes` names how the per-cell planes are spelled, and it is stated rather than \
            inferred: `json` is one number or `null` per cell; `f16` moves `max_db` into \
            `grid.planes.max_db` as base64 of little-endian IEEE binary16 (its destination is an \
            R16F texture, so nothing that reaches a screen is lost) and leaves `occupancy_max`, \
            `coverage` and `frames` as JSON arrays, because for those three JSON is the SMALLER \
            spelling (T-533). A reader that does not know the name served must refuse the tile, \
            not guess: a plane decoded against the wrong type is a measurement invented.",
    })
}

/// A measurement value on the wire: a finite number, or `null`. **`null` is *not observed*, never
/// quiet** (C26) — there is no zero here for anything to read as a level.
pub(crate) fn num(x: f32) -> Value {
    if x.is_finite() { json!(x) } else { Value::Null }
}

/// The one cell every cell of a uniform grid is, stated once instead of enumerated (T-461).
///
/// Written through the same [`num`] the per-cell arrays use, from a real [`OverviewCell`], so the
/// short form cannot say something the long form would not have said about the same cell.
fn uniform_cell_json(c: &hk_store::OverviewCell) -> Value {
    json!({
        "max_db": num(c.max_db),
        "occupancy_max": num(c.occupancy_max),
        "coverage": c.coverage,
        "frames": c.frames,
        "observed": c.observed(),
        "rule": "every cell of this grid holds exactly this, so the per-cell arrays are omitted \
            (T-461). `max_db: null` is the ABSENCE of a level, never a level of zero — and grey is \
            not decided here in any case: `coverage` decides it, cell by cell, and this grid says \
            only that the pyramid holds nothing for any of them.",
    })
}

/// The grid of a tile the **coverage map** answered: `cells × cells` of nothing (T-461).
///
/// Every field is what [`read_level`] would have produced for the same address had it run — an
/// [`Overview`] of [`OverviewCell::UNOBSERVED`], `observed_cells` zero, no `range_db`, and the unit
/// `read_level` falls back to when no level answered. `an_unobserved_tile_read_produces_exactly_the_constants_the_short_circuit_serves`
/// asserts that equality against a real store read, so this is a *cheaper spelling* of the full
/// path's answer and not a second answer.
fn unobserved_grid_json(key: &TileKey, planes: Planes) -> Value {
    let o = Overview {
        unit: hk_model::PowerUnit::Dbfs,
        nt: key.cells,
        nf: key.cells,
        t0_ns: key.region.t0_ns,
        t_cell_ns: key.t_cell_ns as f64,
        f_lo_hz: key.region.freq.lo_hz,
        f_cell_hz: key.f_cell_hz,
        cells: Vec::new(),
        observed_cells: 0,
        src_nt: 0,
        src_nf: 0,
        range_db: None,
    };
    json!({
        "nt": o.nt,
        "nf": o.nf,
        "t0_s": o.t0_ns as f64 / 1e9,
        "t_cell_s": o.t_cell_ns / 1e9,
        "f_lo_hz": o.f_lo_hz,
        "f_cell_hz": o.f_cell_hz,
        // Stated here too (T-533), though this grid carries no plane in either spelling: a reader
        // that has to look at which fields are present to learn the encoding is the reader that
        // decodes the wrong one when a field is legitimately missing.
        "encoding": encoding_json(planes),
        // The four per-cell arrays are ABSENT, not empty: an empty array would read as a grid of
        // no cells, which is a different claim from a grid of cells that hold nothing.
        "uniform": uniform_cell_json(&hk_store::OverviewCell::UNOBSERVED),
        "cells": o.nt * o.nf,
        "observed_cells": 0,
        "range_db": Value::Null,
        "unit": o.unit,
        "percentiles": "unknown: a de-welded fold cannot split a tile's per-frequency histogram \
            across parent time cells, so no percentile is carried (T-434). The noise-floor \
            distribution stays a scheme-1 question, asked through /api/history.",
        "semantics": crate::query::overview_semantics_json(&o),
    })
}

fn grid_json(o: &Overview, planes: Planes) -> Value {
    // T-533: `max_db` in the spelling the caller asked for. The two are the same values in the same
    // order — `the_f16_plane_carries_the_same_values_the_json_array_does` asserts that cell for
    // cell — and the one that is absent is ABSENT, never an empty array: a grid of no cells is a
    // different claim from a grid whose cells are spelled elsewhere.
    let mut v = json!({
        "nt": o.nt,
        "nf": o.nf,
        "t0_s": o.t0_ns as f64 / 1e9,
        "t_cell_s": o.t_cell_ns / 1e9,
        "f_lo_hz": o.f_lo_hz,
        "f_cell_hz": o.f_cell_hz,
        "encoding": encoding_json(planes),
        "occupancy_max": Value::Array(o.cells.iter().map(|c| num(c.occupancy_max)).collect()),
        "coverage": Value::Array(o.cells.iter().map(|c| json!(c.coverage)).collect()),
        "frames": Value::Array(o.cells.iter().map(|c| json!(c.frames)).collect()),
        "cells": o.cells.len(),
        "observed_cells": o.observed_cells,
        "range_db": o.range_db.map(|(lo, hi)| json!({"lo": lo, "hi": hi})),
        "unit": o.unit,
        // De-welding costs the percentiles (T-434): a tile keeps one histogram per frequency cell
        // over the whole tile, which is the parent cell's histogram only under the weld. So
        // `p_low_db`/`floor_db` are not offered here rather than approximated.
        "percentiles": "unknown: a de-welded fold cannot split a tile's per-frequency histogram \
            across parent time cells, so no percentile is carried (T-434). The noise-floor \
            distribution stays a scheme-1 question, asked through /api/history.",
        "semantics": crate::query::overview_semantics_json(o),
    });
    let g = v.as_object_mut().expect("object");
    match planes {
        // Row-major, time then frequency. `null` is **not observed**, never quiet (C26).
        Planes::Json => {
            g.insert(
                "max_db".into(),
                Value::Array(o.cells.iter().map(|c| num(c.max_db)).collect()),
            );
        }
        Planes::F16 => {
            g.insert(
                "planes".into(),
                json!({
                    "max_db": f16_plane(
                        o.cells.iter().map(|c| c.max_db),
                        o.cells.len(),
                        crate::query::scale_str(o.unit),
                    ),
                    "rule": "the typed spelling of the planes it names; every plane NOT named here \
                        is beside it as a JSON array, and `grid.encoding.planes` says which \
                        spelling this answer used.",
                }),
            );
        }
    }
    v
}

/// The **shadow** plane (T-519): each column's most-recent-known value, carried down the tile's
/// rows wherever the tile itself holds none — the fog-of-war tier.
///
/// # What it is, and what it is not
///
/// A band swept and then departed is not grey: it was observed, and the newest thing known about it
/// is a real measurement. It is also not *live*: the value is a measurement of `last_t_s` and
/// earlier, carried forward, and the wire says when. So it is its own honesty tier — **last-known /
/// stale** (`docs/adr/0020`) — beside the three `/api/tiles` already serves, and it changes the
/// meaning of none of them. **Grey is still decided by `coverage` alone**: a column with no run here
/// is one no retained measurement reaches, and a cell the coverage plane calls unobserved with no
/// run over it stays grey.
///
/// # Where the value comes from
///
/// Down each column the carried value starts as the newest one **before `t0`** — read first at the
/// tile's OWN level in the tile's own store ([`hk_store::Pyramid::last_known_search_at`], T-911), so
/// it is the very cell the band's last live row was drawn with and the shadow keeps that row's
/// colour; a value found at any other level is a max-hold over a different box, which is exactly how
/// a departed band's shadow used to change colour at the first tile boundary after it was left. For
/// the columns that level does not reach, it is then read from the
/// spectrum-history pyramid by [`hk_store::Pyramid::last_known_search`] — a query over tiles it
/// already holds, newest-first and fine-to-coarse, so nothing is maintained for it and capture pays
/// nothing (T-453) — and is **replaced by this tile's own value** at every row where the grid holds
/// one. A row where the grid holds a value gets no run: the shadow never stands in for a
/// measurement. Rows at or after the store's newest frame get no run either — except, since
/// T-881, where the tune record reaches past it and the coverage plane says the radio was not
/// looking: the fold trails capture, and those rows are a departed band's newest.
///
/// # Every gap in an observed column, not only the ones after a sample (T-527)
///
/// A column this tile observes only part-way down used to leave the rows **above** its first sample
/// grey whenever the search found nothing older — and grey claims *nothing ever looked here*, which
/// for that column is false. So `carry_forward` fills the head of such a column with its
/// **first-ever** sample, marked `backward` on the wire and distinguishable there from every
/// forward carry, because the two are different claims: *what it looked like when we last saw it*
/// against *when we first saw it*. Nothing else in this plane ever reads backward in time, and a
/// column with neither a sample nor an older value still carries no run — grey is its right answer.
///
/// This costs the **coverage short-circuit** nothing: that path passes `grid: None`, and with no
/// sample in the grid there is no first-ever sample to read back from. The added work on the full
/// path is inside the row walk `carry_forward` already does over the tile's own cells, with no
/// extra source cell read and no change to the search T-523 budgeted.
///
/// # The search's lock holds
///
/// One step per history lock hold, and the **whole search** at most
/// `cells × `[`SHADOW_SEARCH_ROWS`] source cells,
/// capped by [`TILE_MAX_SHADOW_SOURCE_CELLS`] (T-523). That total is a quarter of
/// [`TILE_MAX_SOURCE_CELLS`], the *single-hold* bound, so no hold this search takes can come near
/// lengthening one. T-519's original bounds were the tile read's own, which made the coverage
/// short-circuit — the route's cheapest answer — one of its most expensive. A budget costs the
/// shadow *resolution*, never reach: see [`TILE_MAX_SHADOW_SOURCE_CELLS`].
struct Shadow {
    runs: Vec<hk_store::ShadowRun>,
    known: hk_store::LastKnown,
    store: TileStore,
    /// T-911: the search pinned to the tile's own level in the tile's own store, when it ran, and
    /// that store's per-level `(f_cell_hz, t_cell_ns)`. Its values are carried in `known` for every
    /// column `from_own` marks.
    own: Option<OwnSearch>,
    /// Per column: whether `known`'s value came from `own` rather than from `store`'s search.
    from_own: Vec<bool>,
    edge_ns: Option<i64>,
    /// How far past `edge_ns` runs may reach over cells the coverage plane calls unobserved
    /// (T-881): the answer's own `as_of_s`, or `None` when it reaches no further than the edge.
    reach_ns: Option<i64>,
    chunks: usize,
    elapsed_ms: f64,
    /// Source cells this search was allowed (T-523), on the wire as `search.max_source_cells`.
    budget: usize,
    /// Per level of the searched store: `(f_cell_hz, t_cell_ns)`.
    levels: Vec<(f64, i64)>,
}

/// The store the search before `t0` reads: the spectrum-history pyramid when this server has one,
/// because its ladder is time-deep (seconds → days, each coarse level sealed eagerly); a view
/// lattice's nodes stop at seconds and its coarse ones exist only once materialised. A server with
/// no spectrum history searches the tile's own store, at its level 0.
fn shadow_store(state: &ApiState, tile: TileStore) -> TileStore {
    if state.history.is_some() || state.floor.is_some() {
        TileStore::Main
    } else {
        tile
    }
}

/// The level [`tile_read`] would answer `key` from first — the finest affordable one — or `None`
/// when none is (T-911). A tile the coverage map answers on its own is never read, so this is how
/// its shadow finds the level its neighbours' live rows were drawn at.
fn answering_level(
    state: &ApiState,
    store: TileStore,
    key: &TileKey,
) -> Result<Option<u8>, ApiError> {
    with_tile_history(state, store, |p| {
        Ok(read_affordable_levels(p.geometry(), key)
            .into_iter()
            .find(|&l| fold_affordable(p, key, l))
            .and_then(|l| u8::try_from(l).ok()))
    })
}

/// The last-known search pinned to a tile's own level (T-911): see [`Shadow`].
struct OwnSearch {
    store: TileStore,
    level: u8,
    known: hk_store::LastKnown,
    /// Per level of `store`: `(f_cell_hz, t_cell_ns)`.
    levels: Vec<(f64, i64)>,
}

/// Levels of `store`'s geometry as `(f_cell_hz, t_cell_ns)`.
fn store_levels(state: &ApiState, store: TileStore) -> Result<Vec<(f64, i64)>, ApiError> {
    with_tile_history(state, store, |p| {
        Ok(p.geometry()
            .levels
            .iter()
            .map(|g| (g.f_cell_hz, g.t_cell_ns))
            .collect())
    })
}

/// Runs a last-known search to completion, one lock hold per step. Returns it and the holds taken.
fn run_search(
    state: &ApiState,
    store: TileStore,
    mut search: hk_store::LastKnownSearch,
) -> Result<(hk_store::LastKnown, usize), ApiError> {
    let mut chunks = 0;
    while !search.done() {
        // One lock hold per step, released in between, exactly as the tile read's chunks are.
        with_tile_history(state, store, |p| {
            p.last_known_step(&mut search)
                .map_err(|e| ApiError::new(500, format!("last-known search failed: {e}")))
        })?;
        chunks += 1;
    }
    Ok((search.finish(), chunks))
}

/// `own_level` is the level of `tile_store` whose cells are this tile's — the level that answered
/// the grid, or on the coverage short-circuit the address's own store node — or `None` when there
/// is none. See [`Shadow`] for why the value before the tile is read there first (T-911).
fn shadow(
    state: &ApiState,
    tile_store: TileStore,
    key: &TileKey,
    grid: Option<&Overview>,
    own_level: Option<u8>,
    overlay: &crate::coverage::TileOverlay,
) -> Result<Shadow, ApiError> {
    let started = std::time::Instant::now();
    let store = shadow_store(state, tile_store);
    let (t0, t1, n) = (key.region.t0_ns, key.region.t1_ns, key.cells);
    // T-523: budgeted against the TILE, not against the store. See [`TILE_MAX_SHADOW_SOURCE_CELLS`]
    // for why the tile read's own budget was the wrong scale on the coverage short-circuit.
    let budget = n
        .saturating_mul(SHADOW_SEARCH_ROWS)
        .clamp(1, TILE_MAX_SHADOW_SOURCE_CELLS);
    let latest = |s: TileStore| {
        with_tile_history(state, s, |p| {
            Ok(p.latest_frame_end().map(Timestamp::as_unix_nanos))
        })
    };
    let seed_edge = latest(store)?;
    let edge_ns = match (seed_edge, latest(tile_store).unwrap_or(None)) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    // What the tile itself says about `[t0, t1)`: per column, the first row holding a value. That
    // is what lets a coarse cell straddling `t0` be used for a column — its frames after `t0` would
    // be in this tile, and there are none.
    let first_after: Vec<i64> = (0..n)
        .map(|f| {
            grid.filter(|g| g.nt == n && g.nf == n)
                .and_then(|g| (0..n).find(|&r| g.cells[r * n + f].observed()))
                .map_or(i64::MAX, |r| t0 + r as i64 * key.t_cell_ns)
        })
        .collect();
    let guard = hk_store::StraddleGuard {
        first_after,
        // Known to the end of the data when the newest frame is inside the tile: nothing exists
        // after it to have been missed.
        known_until_ns: if seed_edge.is_none_or(|e| e <= t1) {
            i64::MAX
        } else {
            t1
        },
    };
    // T-911: first, the tile's OWN level in the tile's OWN store — the cells the band's last live
    // row was drawn with. See [`Shadow`].
    let mut chunks = 0;
    let own = match own_level {
        Some(l) => {
            let search = with_tile_history(state, tile_store, |p| {
                Ok(p.last_known_search_at(
                    usize::from(l),
                    key.region.freq,
                    Timestamp::from_unix_nanos(t0),
                    n,
                    key.t_cell_ns,
                    budget,
                    budget,
                ))
            })?;
            let (k, c) = run_search(state, tile_store, search)?;
            chunks += c;
            Some(OwnSearch {
                store: tile_store,
                level: l,
                known: k,
                levels: store_levels(state, tile_store)?,
            })
        }
        None => None,
    };
    let levels = store_levels(state, store)?;
    let own_found: Vec<bool> = match &own {
        Some(o) => o
            .known
            .cells
            .iter()
            .map(hk_store::LastKnownCell::found)
            .collect(),
        None => vec![false; n],
    };
    // Then the spectrum-history ladder, for the columns the tile's own level does not reach: it is
    // time-deep, so a band departed long ago still carries a value — at that ladder's cells, which
    // `sources` states per run.
    let mut known = if own_found.iter().all(|&f| f) {
        // Every column resolved at the tile's own level: the ladder is not searched, and its
        // search block states no stage and no cell read.
        let mut k = own
            .as_ref()
            .map(|o| o.known.clone())
            .expect("found implies searched");
        k.stages.clear();
        k.source_cells = 0;
        k.searched_from_ns = k.before_ns;
        k
    } else {
        let search = with_tile_history(state, store, |p| {
            Ok(p.last_known_search(
                key.region.freq,
                Timestamp::from_unix_nanos(t0),
                n,
                Some(guard),
                // The per-hold bound is the whole budget, which is already a quarter of
                // [`TILE_MAX_SOURCE_CELLS`]: slicing a read that small into smaller holds would
                // only add lock acquisitions. The search still takes several holds — one per
                // stage/slice — but no single one can exceed the route's per-hold cap.
                budget,
                budget,
            ))
        })?;
        let (k, c) = run_search(state, store, search)?;
        chunks += c;
        k
    };
    if let Some(OwnSearch { known: k, .. }) = &own {
        for (f, cell) in known.cells.iter_mut().enumerate() {
            if own_found[f] {
                *cell = k.cells[f];
            }
        }
    }
    // T-881: past the store's newest FOLDED frame, as far as the tune record reaches — the same
    // `as_of_s` this answer's coverage serves — over the cells that record says the radio was not
    // looking at. The fold trails capture, so without this a departed band's newest rows carried
    // no run: coverage `unobserved`, no shadow, drawn as THE grey.
    let reach_ns = overlay
        .as_of_ns()
        .filter(|&r| edge_ns.is_some_and(|e| r > e));
    let mask = reach_ns.and_then(|_| overlay.unobserved_mask(&key.device));
    let beyond = reach_ns.zip(mask.as_deref());
    let runs = known.carry_forward_to(grid, key.t_cell_ns as f64, n, edge_ns, beyond);
    Ok(Shadow {
        runs,
        known,
        store,
        own,
        from_own: own_found,
        edge_ns,
        reach_ns: beyond.map(|(r, _)| r),
        chunks,
        elapsed_ms: started.elapsed().as_secs_f64() * 1e3,
        budget,
        levels,
    })
}

pub(crate) fn store_name(s: TileStore) -> &'static str {
    match s {
        TileStore::View => "view-lattice",
        TileStore::Main => "spectrum-history",
    }
}

/// The `shadow` block: runs as parallel arrays, a source table, and the search that found them.
fn shadow_json(sh: &Shadow, tile_level: Option<u8>) -> Value {
    let s_of = |ns: i64| ns as f64 / 1e9;
    // `src` indexes this table: 0 is this tile's own grid, then one entry per store level a value
    // before the tile came from — so a run's frequency and time resolution are stated, never implied.
    let mut sources = vec![json!({
        "from": "this-tile",
        "level": tile_level,
        "statement": "a value this tile's own grid holds: carried DOWN the rows below it after the \
            band departed (fill \"forward\"), or — for the rows above the column's FIRST-EVER \
            sample, where nothing older exists — that first sample carried UP (fill \"backward\", \
            T-527). Which one a run is, is `fill[i]`, never inferred from this entry.",
    })];
    // Keyed by (from the tile's own-level search?, level): the two searches may read different
    // stores, whose level numbers are not comparable (T-911).
    let mut src_of_level: Vec<((bool, u8), usize)> = Vec::new();
    let mut src = Vec::with_capacity(sh.runs.len());
    for r in &sh.runs {
        src.push(match r.level {
            None => 0,
            Some(l) => {
                let own = sh.from_own.get(r.f).copied().unwrap_or(false);
                match src_of_level.iter().find(|(x, _)| *x == (own, l)) {
                    Some(&(_, i)) => i,
                    None => {
                        let (store, levels) = match (&sh.own, own) {
                            (Some(o), true) => (o.store, &o.levels),
                            _ => (sh.store, &sh.levels),
                        };
                        let (f_cell, t_cell) =
                            levels.get(usize::from(l)).copied().unwrap_or_default();
                        sources.push(json!({
                            "from": "before-tile",
                            "store": store_name(store),
                            "level": l,
                            "f_cell_hz": f_cell,
                            "t_cell_s": s_of(t_cell),
                            "search": if own { "own-level" } else { "ladder" },
                        }));
                        src_of_level.push(((own, l), sources.len() - 1));
                        sources.len() - 1
                    }
                }
            }
        });
    }
    let k = &sh.known;
    let stage_json = |s: &hk_store::LastKnownStage| {
        json!({
            "level": s.level,
            "from_s": s_of(s.from_ns),
            "to_s": s_of(s.to_ns),
            "source_cells": s.source_cells,
            "found": s.found,
            "skipped": s.skipped,
        })
    };
    let own_json = sh.own.as_ref().map(|own| {
        let o = &own.known;
        json!({
            "store": store_name(own.store),
            "level": own.level,
            "columns_found": o.found(),
            "columns_used": sh.from_own.iter().filter(|&&f| f).count(),
            "source_cells": o.source_cells,
            "stages": o.stages.iter().map(stage_json).collect::<Vec<_>>(),
            "rule": "T-911: searched FIRST, at the tile's own level in the store that answers the                 tile, so a carried value is a cell of exactly the time-frequency box the band's                 last live row was drawn with: the shadow keeps the colour that row had. Newest                 block first, skipping blocks that hold nothing over this tile for free; the                 ladder (`search`) is read only for the columns this does not resolve.",
        })
    });
    let unsearched: Vec<Value> = k
        .stages
        .iter()
        .chain(sh.own.iter().flat_map(|o| o.known.stages.iter()))
        .filter(|s| s.skipped)
        .map(|s| json!([s_of(s.from_ns), s_of(s.to_ns)]))
        .collect();
    let own_cells = sh.own.as_ref().map_or(0, |o| o.known.source_cells);
    json!({
        "encoding": "column-runs",
        "runs": sh.runs.len(),
        "f": sh.runs.iter().map(|r| r.f).collect::<Vec<_>>(),
        "row": sh.runs.iter().map(|r| r.row).collect::<Vec<_>>(),
        "rows": sh.runs.iter().map(|r| r.rows).collect::<Vec<_>>(),
        "last_db": sh.runs.iter().map(|r| num(r.max_db)).collect::<Vec<_>>(),
        "last_t_s": sh.runs.iter().map(|r| s_of(r.t_ns)).collect::<Vec<_>>(),
        "src": src,
        "sources": sources,
        // T-527. Which WAY in time the run reads, as a code into `fills` — the same shape as
        // `coverage.states`, and for the same reason: the two are different claims about the same
        // shadow, and a client that cannot tell them apart is being told something false about one
        // of them. `fills[0]` is the default a pre-T-527 reader already assumes.
        "fill": sh
            .runs
            .iter()
            .map(|r| u8::from(r.fill == hk_store::ShadowFill::Backward))
            .collect::<Vec<_>>(),
        "fills": ["forward", "backward"],
        "backward_runs": sh
            .runs
            .iter()
            .filter(|r| r.fill == hk_store::ShadowFill::Backward)
            .count(),
        "edge_s": sh.edge_ns.map(s_of),
        // T-881: how far past `edge_s` a run may reach — this answer's `coverage.horizon.as_of_s`
        // — or null when runs stop at `edge_s`.
        "reach_s": sh.reach_ns.map(s_of),
        "search": {
            "store": store_name(sh.store),
            "before_s": s_of(k.before_ns),
            "searched_from_s": s_of(k.searched_from_ns),
            "columns_found": k.found(),
            "unsearched": unsearched,
            "stages": k.stages.iter().map(stage_json).collect::<Vec<_>>(),
            "own_level": own_json,
            "source_cells": k.source_cells + own_cells,
            "max_source_cells": sh.budget,
            "chunks": sh.chunks,
            "build_ms": (sh.elapsed_ms * 1000.0).round() / 1000.0,
            "rule": "newest-first, fine-to-coarse: each stage reads one level over the part of the \
                past the finer stage above it did not, so the whole retained horizon costs a few \
                hundred rows per column; the search stops when every column has a value or the \
                store holds nothing older. Bounded by `max_source_cells` (T-523: cells x 512 rows, \
                the TILE's scale and not the store's — the whole search's total is a quarter of what \
                `resolution.budget` allows ONE lock hold). The budget is paid in RESOLUTION, \
                never in reach: a stage it cannot afford is skipped and the next COARSER level \
                covers that window, which is why `sources[].level` is on the wire per run. A window \
                in `unsearched` was NOT read (over budget, or its coarse cell is not folded yet): a \
                column with no run is unobserved in [searched_from_s, before_s) OUTSIDE those \
                windows, and nothing is claimed about earlier.",
        },
        "rule": "the LAST-KNOWN tier (docs/adr/0020), NOT a measurement of the row it is drawn on. \
            Run i covers rows [row[i], row[i] + rows[i]) of column f[i], in `grid.order`'s axes; \
            last_db[i] is a max-hold measured at last_t_s[i] (absolute capture time), resolved at \
            sources[src[i]]'s cells. EVERY time gap in a column that was ever observed is filled \
            (T-527), and fill[i] says which way the value was read: \"forward\" — the nearest PAST \
            sample, last seen at last_t_s[i], which is at or before the run — or \"backward\" — the \
            column's FIRST-EVER sample, first seen at last_t_s[i], which is AFTER the run and is \
            the only value in this plane read backward in time. A backward run exists only above a \
            column's first sample and only where the search found nothing older; everything else is \
            forward. Either way last_t_s[i] is the boundary NEAREST the run, so |row time - \
            last_t_s[i]| is the smallest age the evidence supports. A row where `grid` holds a \
            value is never covered: a shadow never replaces a measurement. Rows at or after \
            `edge_s` (the newest FOLDED frame) are covered only up to `reach_s` (the tune \
            record's reach, this answer's `coverage.horizon.as_of_s`; null = not at all), and only \
            where the selected coverage plane says \"unobserved\": the fold trails capture, and a \
            departed band's rows up to where the record reaches are time the radio spent \
            elsewhere (T-881). A column meeting a not-unobserved cell past `edge_s` stops there. \
            A column with NO sample and NO older \
            value carries NO run at all — it was never observed. GREY IS UNCHANGED AND IS STILL \
            DECIDED BY `coverage` ALONE: draw a shadow only where the coverage plane says \
            \"unobserved\", and a cell with no run over it stays grey — no retained measurement \
            reaches it. The plane is not device-scoped: the pyramid is not, and this carries its \
            values.",
    })
}

fn key_json(key: &TileKey) -> Value {
    json!({
        // Coverage is device-local (T-259/T-305, docs/16 §6.3), so the device is part of the KEY
        // and never part of a cell — and `any` keeps `named: false` so a union can never wear one
        // radio's identity.
        "device": key.device,
        "device_named": key.named_device(),
        "scheme": key.lattice.name,
        "level_f": key.level_f,
        "level_t": key.level_t,
        "f_index": key.f_index,
        "t_index": key.t_index,
        "cells": key.cells,
    })
}

fn axes_json(key: &TileKey, ceiling: (usize, usize)) -> Value {
    json!({
        "frequency": {
            "levels": key.lattice.f_cells_hz.len(),
            "max_level": ceiling.0,
            "cell_hz": key.f_cell_hz,
            "tile_hz": key.f_cell_hz * key.cells as f64,
        },
        "time": {
            "levels": key.lattice.t_cells_ns.len(),
            "max_level": ceiling.1,
            "cell_s": key.t_cell_ns as f64 / 1e9,
            "tile_s": key.t_cell_ns as f64 * key.cells as f64 / 1e9,
        },
        // T-482. `levels` and `max_level` answer DIFFERENT questions and a client that conflates
        // them addresses a node that is named and cannot be built: how many levels the address
        // lattice NAMES, against how far up it can actually be READ. They differ whenever the store
        // ladder is shallower than the address lattice, which on the shipped geometry is a 12 x 15
        // lattice over a 4 x 4 store.
        "readable": "`max_level` is a CEILING ON READING, per axis, and it is a BOX: every address \
            with level_f <= frequency.max_level AND level_t <= time.max_level can be served. \
            Beyond it this route answers 400, because the store behind the lattice cannot back the \
            tile — `levels` names the addresses, `max_level` is the reachable part of them. It is \
            computed from the open pyramid's own geometry against both of this route's work bounds \
            (the per-tile source-cell budget in `resolution.budget`, and the store's fold budget), \
            on the WORST store either could meet, so it does not move as the store fills. \
            The servable set is an AREA constraint and not a box — work goes as tile_hz x tile_s, \
            so the real bound is an anti-diagonal in (level_f, level_t) — and a box inscribed in it \
            cannot reach both far corners. What a box loses is the very-wide-AND-very-tall pairs; \
            the box stated is the one whose corner tile covers the most area, and the tie among \
            those goes to frequency. IT IS STATED FOR A 256-CELL TILE, this route's own unit, and \
            NOT for this answer's `cells`: the bound is on area, so a ceiling quoted per answer \
            would be a lie the moment a client probed cheaply and drew at full size — which is \
            exactly what a client does. 256 is also the widest tile this route accepts, so the \
            pair can never over-claim for a smaller one; a caller using `cells` below 256 can read \
            further than this says, and gives up that reach for a number it can cache.",
        // The store level whose cells are EXACTLY this tile's, when the scheme has one. `null` on
        // the view lattice is not an error: it says this pair is off a welded ladder's diagonal, so
        // the tile is folded rather than read whole (T-434).
        "store_node": key.store_node,
        "independent": "level_f and level_t are independent coordinates: changing one moves only \
            its own axis's cell. The de-welded lattice is the point — a welded ladder is its \
            DIAGONAL, and `store_node` is null wherever this pair is off it.",
    })
}

/// `GET /api/tiles`.
///
/// **Admission first** (T-630): the slot is taken for the *named client* before any work, and the
/// answer marks that client as drawn. See [`TileAdmission`] for why a slot is not
/// first-come-first-served.
pub fn tiles_json(state: &ApiState, q: &Params) -> Result<Value, ApiError> {
    const ALLOWED: [&str; 10] = [
        "device", "scheme", "level_f", "level_t", "f_index", "t_index", "cells", "planes",
        "client", "token",
    ];
    if let Some((k, _)) = q.iter().find(|(k, _)| !ALLOWED.contains(&k.as_str())) {
        return Err(bad(&format!(
            "unknown parameter {k:?} (allowed: device, scheme, level_f, level_t, f_index, \
             t_index, cells, planes, client)"
        )));
    }
    // T-700: the plane selection is validated BEFORE admission, so a misspelled `planes` is a 400
    // that never takes a slot from the client's share.
    let planes = parse_planes(q)?;
    let client = client_id(q);
    let slot = match state.tile_admission.acquire(&client) {
        Ok(slot) => slot,
        // T-581: the cap is a PRODUCER cap. A sealed tile the hot-tile cache already holds is
        // answered from RAM with no pyramid read, so refusing it would only turn a cheap answer
        // into a client-side halving of its operating limit (`tilecache.ts`'s AIMD) behind a
        // couple of slow coarse producers. See [`hot_hit_unslotted`].
        Err(share) => {
            return hot_hit_unslotted(state, q, planes, &client, share)
                .ok_or_else(|| too_many_in_flight(share));
        }
    };
    let answer = tile_body(state, q, &slot, planes);
    // Served means *drawn*: only an answer disarms this client's bootstrap reserve.
    if answer.is_ok() {
        slot.mark_served();
    }
    answer
}

fn tile_body(
    state: &ApiState,
    q: &Params,
    slot: &TileSlot,
    planes: Planes,
) -> Result<Value, ApiError> {
    let store = tile_store(state, q);
    let (key, ceiling, readable, sealed) = with_tile_history(state, store, |p| {
        let key = parse_key(p.geometry(), q)?;
        let ceiling = state.ceiling_memo.ceiling(p, &key.lattice);
        let readable = servable(p, &key);
        // T-574: sealedness is a fact about the ADDRESS, not about what answered it — a tile's
        // whole time extent can never change again once the watermark has passed its end, because
        // a frame landing before that end is by definition late and dropped (the same rule
        // `Pyramid::materialize_tile` uses to decide sealed-vs-derived, `docs/16` §5.2/§5.3
        // generalised from one store level's block to this tile's own extent). Computed here, from
        // the pyramid's own state, and threaded out to `http.rs` rather than re-derived from a
        // guess or the answering level/age.
        let sealed = p.watermark().as_unix_nanos() >= key.region.t1_ns;
        Ok((key, ceiling, readable, sealed))
    })?;
    let started = std::time::Instant::now();
    let max_live = crate::http::max_live_span_hz(state);
    // T-572: SEALED ONLY. A sealed tile's whole extent has passed the watermark and can never
    // change again; a live tile at the growing edge changes on every arriving row, so it is never
    // looked up here and never inserted below — it is always re-read, which is what keeps the
    // live view appending rows in real time. The slot is already held (T-630 admission ran in
    // `tiles_json`), so a hit costs a lock and a clone and no history hold at all.
    let cache_key = state
        .tile_cache
        .as_ref()
        .filter(|_| sealed)
        .map(|_| hot_tile_key(&key, store, planes, max_live));
    let epoch = coverage_epoch(state);
    if let (Some(c), Some(k)) = (state.tile_cache.as_ref(), cache_key.as_ref())
        && let Some(mut v) = c.get(k, epoch)
    {
        // The answer is the cached one, but the measurement of what THIS read cost, and whose
        // share it was admitted under (T-630), is this read's — `http.rs` strips exactly these
        // before hashing a response into an ETag, so a hit and a miss still validate identically.
        stamp_read_cost(&mut v["cost"], started.elapsed().as_secs_f64() * 1e3, slot);
        v["cost"]["served_from"] = json!("hot-tile-cache");
        return Ok(v);
    }
    let window = key.window();
    // T-467: the compact plane-table form. The per-cell form this route used to serve was 99 % of a
    // live tile's 19.34 MB body, duplicated between `any` and `devices[0]`, for a `state` field the
    // renderer reads and nothing else. `TileOverlay` computes each distinct plane once and serves
    // it once.
    //
    // T-461: and it is computed **first**, because it is what decides grey. When the selected plane
    // is `unobserved` over the tile's whole extent, the answer is already complete and the pyramid
    // read below is 65 536 source cells spent to confirm it.
    let overlay =
        crate::coverage::TileOverlay::collect(state, key.region.freq, window, key.cells, key.cells);
    let uniform = overlay.uniform_state(&key.device);
    let coverage = overlay.to_json(&key.device, key.named_device());
    // T-515 (folded into T-507): **the shortcut is a cheaper spelling of the full path, including
    // its refusals.** An address no store level can back is refused whatever its coverage — the
    // `tile_read` below answers it with the full path's own 400. Otherwise whether an address is
    // servable would depend on what the radio happened to sample there: the same address would
    // answer 200 over an unobserved band and 400 once the band became observed, and the declared
    // ceiling (`axes.*.max_level`, a pure function of geometry) would stop being the bound a client
    // can cache. Until T-507 this was hidden by accident: a young server's past was `"unknown"`,
    // which never short-circuits, so the walk above the ceiling reached the refusal anyway.
    if uniform == Some("unobserved") && readable {
        // T-519: a tile the coverage map greys end to end is exactly where a departed band's
        // shadow lives, so the last-known search runs here too — against no grid, because the
        // coverage map just said no tune touched this tile.
        let own = answering_level(state, store, &key)?;
        let sh = shadow(state, store, &key, None, own, &overlay)?;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1e3;
        let mut v = unobserved_tile_json(
            &key,
            store,
            ceiling,
            coverage,
            max_live,
            ReadDiagnostics { elapsed_ms, slot },
            sealed,
            planes,
        );
        v["shadow"] = shadow_json(&sh, None);
        return Ok(cache_put(state, &cache_key, epoch, v));
    }
    let r = tile_read(state, store, &key)?;
    let sh = shadow(state, store, &key, Some(&r.grid), Some(r.level), &overlay)?;
    let levels = with_tile_history(state, store, |p| Ok(p.geometry().n_levels()))?;
    let source = tier(&key, &r, max_live);
    let elapsed_ms = started.elapsed().as_secs_f64() * 1e3;
    let v = json!({
        // T-574: whether this tile's own time extent has fully passed the watermark, so it can
        // never change again — `http.rs` reads this to decide the ETag / Cache-Control, never a
        // timestamp or an age. false at the live edge (or anywhere still inside the retained
        // window), always, so a growing tile is never cached as immutable.
        "sealed": sealed,
        "key": key_json(&key),
        "extent": {
            "f_lo_hz": key.region.freq.lo_hz,
            "f_hi_hz": key.region.freq.hi_hz,
            "f_cell_hz": key.f_cell_hz,
            "t0_s": key.region.t0_ns as f64 / 1e9,
            "t1_s": key.region.t1_ns as f64 / 1e9,
            "t_cell_s": key.t_cell_ns as f64 / 1e9,
            "nt": key.cells,
            "nf": key.cells,
        },
        "axes": axes_json(&key, ceiling),
        "grid": grid_json(&r.grid, planes),
        "coverage": coverage,
        "shadow": shadow_json(&sh, Some(r.level)),
        "resolution": {
            "source": source.as_str(),
            "live": source.is_live(),
            "statement": source.statement(),
            // Which tier ANSWERED, not the one the address implies (T-426). A silent fallback
            // would trade one lie for another.
            "answered": {
                "level": r.level,
                "levels": levels,
                "f_cell_hz": r.src_f_cell_hz,
                "t_cell_s": r.src_t_cell_ns as f64 / 1e9,
                "exact_node": key.store_node == Some(r.level as usize),
                // T-439: WHICH pyramid answered. `view-lattice` is the de-welded scheme the live
                // chain writes its finest node of, so a fine-frequency/coarse-time address is a
                // real node rather than a fold out of a ladder's diagonal. There is no separate
                // live path — this says which surface, never whether it was "live".
                "store": match store {
                    TileStore::View => "view-lattice",
                    TileStore::Main => "spectrum-history",
                },
            },
            "candidates": r.candidates,
            "tried": r.tried,
            "fold": {
                "frequency": axis_fold(r.src_f_cell_hz, key.f_cell_hz, r.grid.src_nf, key.cells),
                "time": axis_fold(
                    r.src_t_cell_ns as f64,
                    key.t_cell_ns as f64,
                    r.grid.src_nt,
                    key.cells,
                ),
                "rule": "the served grid is ALWAYS the tile's own cells x cells; the level only \
                    decides what was folded onto it. `folded` reduces a finer measurement (nothing \
                    is invented and nothing is greyed); `replicated` repeats one measured value \
                    across output cells and is the only direction that makes a claim, so it \
                    downgrades the tier to survey-overview.",
            },
            "budget": {
                "max_source_cells_per_lock": TILE_MAX_SOURCE_CELLS,
                "max_source_cells_per_tile": TILE_MAX_TOTAL_SOURCE_CELLS,
                "statement": "these bound WORK, never resolution: the level is chosen \
                    finest-affordable-first, so a budget can only ever move the answer toward a \
                    COARSER source, which replicates and says so — it can never grey a cell a \
                    finer level holds. There is no caller-supplied per-axis cell budget on this \
                    route at all, which is why /api/history's max_f defect (T-437 F2: a 1.5x \
                    tighter frequency budget cost 34x of time resolution and greyed a third of the \
                    window) cannot be expressed here.",
            },
            // T-461: served on the full path too, so "did the coverage map answer this?" is a
            // question the wire answers and `applied: false` names the reason it did not.
            "short_circuit": short_circuit_json(uniform),
            "grey_rule": "grey is decided by `coverage`, never by this block: a level that holds \
                nothing here is a level, and a cell nothing ever sampled is grey. Not-loaded is a \
                third thing and is the client's to draw (docs/16 §5.5).",
        },
        "cost": {
            "build_ms": (elapsed_ms * 1000.0).round() / 1000.0,
            "source_cells": r.source_cells,
            "chunks": r.chunks,
            "in_flight": slot.in_flight(),
            "in_flight_limit": TILE_MAX_IN_FLIGHT,
            // T-630: the cap is server-wide, the SHARE is this client's, and the share is the
            // number a client should operate at. `clients` is how many are reading tiles right
            // now, so a client can see why its share moved.
            "in_flight_share": slot.share().share,
            "in_flight_held": slot.held(),
            "clients": slot.share().clients,
            "client": slot.client(),
            "reserved": slot.share().reserved,
            "fair_share": slot.share().fair,
            "statement": "tile PRODUCTION is the cost this surface is designed against, not \
                rendering: T-437 measured 48 panes at p95 2.2 ms against ~500 ms per tile. \
                `chunks` is the number of history lock holds this tile took.",
        },
    });
    Ok(cache_put(state, &cache_key, epoch, v))
}

// ---------------------------------------------------------------------------------------------
// T-572: the hot-tile LRU.
// ---------------------------------------------------------------------------------------------

/// The cache's byte bound.
///
/// **Bounded is the point, and the bound is in bytes** (T-453): residency must not grow with node
/// count, and a cache sized in *tiles* would, because a tile's size is a property of the grid.
/// Thirty-two mebibytes is a few viewports' worth of `planes=f16` tiles and a small fraction of
/// what one `/api/history` query already allocates; it does not move when the pyramid deepens,
/// when a pane zooms, or when a second client connects.
pub const TILE_CACHE_MAX_BYTES: usize = 32 * 1024 * 1024;

/// The cache's entry bound.
///
/// The byte bound alone would admit an unbounded number of tiny answers — a 7.5 kB
/// coverage-short-circuit tile is 4300 of them inside 32 MiB — and each entry costs a key and a
/// `Value` tree beyond its serialized size. Whichever bound binds first evicts.
pub const TILE_CACHE_MAX_ENTRIES: usize = 256;

/// One cached answer.
struct HotTile {
    body: Value,
    bytes: usize,
    /// Monotonic use stamp; the smallest is the least recently used.
    used: u64,
}

/// An in-memory LRU of **sealed** tile answers, in front of the filesystem (T-572).
///
/// **Only sealed tiles, and that is the whole correctness argument.** A sealed tile's own time
/// extent has fully passed the pyramid's watermark, so a frame landing inside it is by definition
/// late and dropped (the same rule T-574 gives its ETag and its `immutable` cache-control): it can
/// never change again. A LIVE tile at the growing edge changes on every arriving row, and serving
/// a stale one would violate *"the live view renders like a classic SDR waterfall, rows append in
/// real time"* exactly as badly as not serving it at all — so a live tile is never inserted, never
/// looked up, and always re-read. The distinction is in the insert path, not in a timer.
///
/// **What can still change about a sealed tile, and how that is caught.** The grid cannot, but the
/// `coverage` plane beside it is derived from the observation log, which is appended to as capture
/// proceeds and pruned by retention — so the cache carries an **epoch** taken from that log's own
/// counters (`written`, `segments_deleted`), and any movement in either drops every entry. The
/// epoch is read, never written, by this path: those two atomics are already incremented on the
/// capture thread, so reading them costs the reader and nothing costs the writer.
///
/// **The capture thread is not on this path at all.** The cache lives entirely in the HTTP read
/// path: no lock it holds is taken by ingest, and nothing it does runs per arriving row. That is
/// why there is no per-row measurement here — there is no per-row work to measure (T-453).
#[derive(Default)]
pub struct HotTileCache {
    inner: Mutex<HotTileCacheInner>,
}

#[derive(Default)]
struct HotTileCacheInner {
    map: std::collections::HashMap<String, HotTile>,
    bytes: usize,
    clock: u64,
    epoch: (u64, u64),
    hits: u64,
    misses: u64,
    evictions: u64,
    invalidations: u64,
}

impl HotTileCache {
    /// A cached answer for `key`, if one is held at `epoch`.
    fn get(&self, key: &str, epoch: (u64, u64)) -> Option<Value> {
        let mut g = self.inner.lock().ok()?;
        g.reset_if_stale(epoch);
        g.clock += 1;
        let clock = g.clock;
        let Some(e) = g.map.get_mut(key) else {
            g.misses += 1;
            return None;
        };
        e.used = clock;
        let body = e.body.clone();
        g.hits += 1;
        Some(body)
    }

    /// Hold `body` for `key`, evicting the least recently used until both bounds hold.
    fn put(&self, key: String, body: &Value, epoch: (u64, u64)) {
        let Ok(mut g) = self.inner.lock() else { return };
        g.reset_if_stale(epoch);
        let bytes = serde_json::to_vec(body).map(|b| b.len()).unwrap_or(0);
        // A single answer larger than the whole cache is not cached: holding it would evict
        // everything else to no purpose, and the bound must hold unconditionally.
        if bytes > TILE_CACHE_MAX_BYTES {
            return;
        }
        g.clock += 1;
        let clock = g.clock;
        if let Some(old) = g.map.remove(&key) {
            g.bytes -= old.bytes;
        }
        g.map.insert(
            key,
            HotTile {
                body: body.clone(),
                bytes,
                used: clock,
            },
        );
        g.bytes += bytes;
        while g.bytes > TILE_CACHE_MAX_BYTES || g.map.len() > TILE_CACHE_MAX_ENTRIES {
            let Some(victim) = g
                .map
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(e) = g.map.remove(&victim) {
                g.bytes -= e.bytes;
            }
            g.evictions += 1;
        }
    }

    /// The counters, for `/api/status` and for the tests that assert the bound holds.
    pub fn stats_json(&self) -> Value {
        let Ok(g) = self.inner.lock() else {
            return Value::Null;
        };
        json!({
            "entries": g.map.len(),
            "bytes": g.bytes,
            "max_entries": TILE_CACHE_MAX_ENTRIES,
            "max_bytes": TILE_CACHE_MAX_BYTES,
            "hits": g.hits,
            "misses": g.misses,
            "evictions": g.evictions,
            "invalidations": g.invalidations,
            "rule": "SEALED TILES ONLY. A sealed tile's time extent has fully passed the \
                pyramid's watermark, so it can never change again; a live tile at the growing \
                edge changes on every arriving row and is never cached, never looked up and \
                always re-read. `invalidations` counts the times the observation log moved \
                (records written or segments deleted) and every entry was dropped, because the \
                coverage plane beside a sealed grid is derived from that log.",
        })
    }
}

impl HotTileCacheInner {
    fn reset_if_stale(&mut self, epoch: (u64, u64)) {
        if self.epoch != epoch {
            if !self.map.is_empty() {
                self.invalidations += 1;
            }
            self.map.clear();
            self.bytes = 0;
            self.epoch = epoch;
        }
    }
}

/// The observation log's own counters, as the cache's invalidation epoch.
///
/// `written` moves when a record is appended and `segments_deleted` when retention removes one:
/// between them, every way the coverage plane over a past extent can change. With no observation
/// log at all the epoch is constant, which is correct — there is nothing to change.
fn coverage_epoch(state: &ApiState) -> (u64, u64) {
    let Some(obs) = state.observations.as_ref() else {
        return (0, 0);
    };
    let s = obs.stats();
    (
        s.written.load(Ordering::Relaxed),
        s.segments_deleted.load(Ordering::Relaxed),
    )
}

/// Everything that decides a tile's body, as one string.
///
/// The address, the device, the store, the plane spelling, and `max_live` (which chooses the
/// honesty tier this answer states). Anything not in here would be a way for two different answers
/// to share one entry.
fn hot_tile_key(key: &TileKey, store: TileStore, planes: Planes, max_live: Option<f64>) -> String {
    format!(
        "{}|{}|{}|{}|{:?}",
        key_json(key),
        key.device,
        match store {
            TileStore::View => "view",
            TileStore::Main => "main",
        },
        planes.as_str(),
        max_live.map(f64::to_bits),
    )
}

/// Overwrite `cost`'s per-read fields with THIS read's (T-572 over T-630).
///
/// A cached body was built by an earlier read: its wall clock, its in-flight count and — since
/// T-630 — the share, holdings and client name it was admitted under all belong to that read.
/// Serving them on a hit would tell this client someone else's share. Every field set here is one
/// `http.rs` strips before hashing a sealed tile into its ETag.
fn stamp_read_cost(cost: &mut Value, elapsed_ms: f64, slot: &TileSlot) {
    let share = slot.share();
    cost["build_ms"] = json!((elapsed_ms * 1000.0).round() / 1000.0);
    cost["in_flight"] = json!(slot.in_flight());
    cost["in_flight_share"] = json!(share.share);
    cost["in_flight_held"] = json!(slot.held());
    cost["clients"] = json!(share.clients);
    cost["client"] = json!(slot.client());
    cost["reserved"] = json!(share.reserved);
    cost["fair_share"] = json!(share.fair);
}

/// **A sealed, already-cached tile is answered even when every producer slot is out** (T-581).
///
/// [`TILE_MAX_IN_FLIGHT`] exists because tile *production* takes the history lock and competes
/// with ingest. A hot-tile hit does no production: one brief hold to resolve the address and read
/// the watermark (exactly the hold an admitted hit already took), then a RAM lookup. Before T-581
/// such a hit still needed a slot, so while a couple of slow coarse tiles held the cap every
/// already-drawn sealed tile a steady-state poll re-asked for came back `503` — and the client
/// halves its operating limit on each one, which is how T-450 saw 17 268 cancellations against 26
/// resident tiles.
///
/// Only reached **after** admission refused, so an admitted read's accounting (T-630's shares, the
/// bootstrap reserve) is exactly what it was. A LIVE tile is never in the cache (T-572), so it
/// still needs a slot and still meets the cap: the cap keeps binding every read that would touch
/// the store for more than that one hold. `None` means "no cached answer": the caller refuses.
fn hot_hit_unslotted(
    state: &ApiState,
    q: &Params,
    planes: Planes,
    client: &str,
    share: Share,
) -> Option<Value> {
    let started = Instant::now();
    let cache = state.tile_cache.as_ref()?;
    let store = tile_store(state, q);
    let (key, sealed) = with_tile_history(state, store, |p| {
        let key = parse_key(p.geometry(), q)?;
        let sealed = p.watermark().as_unix_nanos() >= key.region.t1_ns;
        Ok((key, sealed))
    })
    .ok()?;
    if !sealed {
        return None;
    }
    let max_live = crate::http::max_live_span_hz(state);
    let k = hot_tile_key(&key, store, planes, max_live);
    let mut v = cache.get(&k, coverage_epoch(state))?;
    let cost = &mut v["cost"];
    cost["build_ms"] = json!((started.elapsed().as_secs_f64() * 1e6).round() / 1000.0);
    cost["in_flight"] = json!(state.tile_admission.in_flight());
    cost["in_flight_share"] = json!(share.share);
    // This read holds no producer slot, and says so.
    cost["in_flight_held"] = json!(0);
    cost["clients"] = json!(share.clients);
    cost["client"] = json!(client);
    cost["reserved"] = json!(share.reserved);
    cost["fair_share"] = json!(share.fair);
    cost["served_from"] = json!("hot-tile-cache");
    state
        .tile_admission
        .hot_answers
        .fetch_add(1, Ordering::Relaxed);
    // Served means drawn, however it was served (T-630).
    state.tile_admission.mark_served(client);
    Some(v)
}

/// Hold `v` in the hot-tile cache when this address earned one (T-572), and hand it back.
///
/// `key` is `None` for a LIVE tile — the insert path is where the sealed/live distinction lives,
/// so there is no way to reach this with a growing tile's body.
fn cache_put(state: &ApiState, key: &Option<String>, epoch: (u64, u64), v: Value) -> Value {
    if let (Some(c), Some(k)) = (state.tile_cache.as_ref(), key.as_ref()) {
        c.put(k.clone(), &v, epoch);
    }
    v
}

// ---------------------------------------------------------------------------------------------
// T-573: one request per viewport, not one per tile.
// ---------------------------------------------------------------------------------------------

/// The most addresses one `GET /api/tiles/batch` may name.
///
/// A viewport is tens of tiles, not hundreds — a 6 GHz-wide pane at the overview lattice was
/// measured at 1780 addresses across the WHOLE canvas (T-484), and no single pane asks for its
/// whole canvas at once. Sixty-four is comfortably above a pane row and far below anything that
/// could turn one request into a long occupation of a connection thread. Over it the route
/// **refuses**, naming the cap, rather than silently answering a prefix: a caller that asked for
/// more than it may have needs to know which addresses it must re-ask for, and the cheapest
/// honest answer is "split it".
pub const TILES_BATCH_MAX_ADDRESSES: usize = 64;

/// The most bytes one batch answer may carry.
///
/// Unlike the address cap this one **truncates** rather than refuses, because its trigger is not
/// the caller's fault: a tile's size is a property of the grid, not of the request, so a legal
/// 64-address batch can be cheap over unobserved spectrum and enormous over a full one. The
/// answer says `truncated: true` and lists every address it did not reach in `remaining`, so the
/// remainder is addressable — the caller asks again for exactly those and makes progress.
///
/// At least one tile is always returned even when it alone exceeds the cap; otherwise a single
/// oversized address would be unfetchable through this route forever.
pub const TILES_BATCH_MAX_BYTES: usize = 8 * 1024 * 1024;

/// `GET /api/tiles/batch` — a viewport's worth of tile addresses in one request (T-573).
///
/// **This is a transport change, never an analysis one.** Each entry's `tile` is byte-identical to
/// what `GET /api/tiles` would have answered for that address on its own: the same `key`, the same
/// independent `(level_f, level_t)` pair, the same `coverage` plane, the same `resolution` block
/// and the same per-tile `cost`. Batching is a way to ask, not a way to summarise, and every
/// per-address fact the canvas depends on survives it.
///
/// **A partial answer is expressible, and that is the point.** A viewport where three tiles have
/// data, one is genuinely unobserved and one was refused is ONE response carrying three 200s, a
/// 200 whose own `coverage` says unobserved, and a 503 — never one status for the set. Collapsing
/// a missing tile into an empty one is precisely the defect the coverage map exists to prevent, so
/// there is no "status" for the batch as a whole beyond the transport's own 200.
///
/// **The coverage short-circuit is untouched.** Each address goes through [`tiles_json`], which
/// answers a uniformly-unobserved tile from the coverage map without ever reaching the generation
/// path (T-461). A batch endpoint that made empty tiles expensive again would be a regression and
/// not a win, so the cheap path is reached by construction: this route adds a loop, not a
/// different tile builder.
///
/// **The in-flight cap is per address, still.** `tiles_json` takes and releases one
/// [`TileSlot`] per address, so a batch of sixty-four holds ONE producer slot at a time, never
/// sixty-four; under contention its later addresses come back as per-address 503s and the caller
/// re-asks for exactly those. It does not widen the cap and it does not queue behind it.
pub fn tiles_batch_json(state: &ApiState, q: &Params) -> Result<Value, ApiError> {
    tiles_batch_json_capped(state, q, TILES_BATCH_MAX_BYTES)
}

/// [`tiles_batch_json`] with the response-size cap as an argument.
///
/// The seam exists so the truncation path is tested **through the route**, at a cap small enough
/// for a fixture to cross, rather than by a separate pure function that could drift from what the
/// route does. The public entry point is the only caller outside tests, and it passes the
/// documented constant.
fn tiles_batch_json_capped(
    state: &ApiState,
    q: &Params,
    max_bytes: usize,
) -> Result<Value, ApiError> {
    const ALLOWED: [&str; 7] = [
        "device",
        "scheme",
        "cells",
        "planes",
        "client",
        "addresses",
        "token",
    ];
    if let Some((k, _)) = q.iter().find(|(k, _)| !ALLOWED.contains(&k.as_str())) {
        return Err(bad(&format!(
            "unknown parameter {k:?} (allowed: device, scheme, cells, planes, client, \
             addresses). \
             `level_f`, `level_t`, `f_index` and `t_index` are per-address and belong in \
             `addresses`, not beside it"
        )));
    }
    // Shared across the batch, because a viewport is drawn from ONE lattice at ONE cell count for
    // ONE device: these are properties of the viewport, and the addresses are what vary within it.
    // A client mixing schemes or devices (a pane and the minimap) issues one batch per group,
    // which is still a small constant per render and keeps every request self-describing.
    // `client` (T-630) is shared for the same reason: one request is one asker, and each address
    // takes its admission slot under that asker's share exactly as a single-tile read would.
    let shared: Vec<(String, String)> = q
        .iter()
        .filter(|(k, _)| {
            matches!(
                k.as_str(),
                "device" | "scheme" | "cells" | "planes" | "client"
            )
        })
        .cloned()
        .collect();
    let raw = q
        .iter()
        .find(|(k, _)| k == "addresses")
        .map(|(_, v)| v.as_str())
        .ok_or_else(|| {
            bad(
                "addresses=<level_f>.<level_t>.<f_index>.<t_index>[,…] names the tiles to answer; \
                 a batch with no addresses is not a cheaper spelling of anything",
            )
        })?;
    let addrs = parse_batch_addresses(raw)?;
    if addrs.len() > TILES_BATCH_MAX_ADDRESSES {
        return Err(bad(&format!(
            "{} addresses is over this route's cap of {TILES_BATCH_MAX_ADDRESSES} per request \
             (refused rather than truncated, so you know exactly which addresses still need \
             asking for): split the viewport and ask again",
            addrs.len()
        )));
    }

    // **Members are answered CONCURRENTLY, not one after another** (T-573 follow-up). The client
    // hands the batch exactly the addresses its in-flight budget would have put on the wire as
    // separate requests, which the route served in parallel on up to [`TILE_MAX_IN_FLIGHT`]
    // slots. Answering them in sequence made one batch cost the SUM of its members, not the
    // slowest — measured in `ui/e2e/live-edge.e2e.mjs`: a following pane's newest rows were still
    // a flat fill 8 s after first draw, because the live-edge tile waited behind every tile beside
    // it. That is the live edge gated on tile generation, which the product forbids.
    //
    // **The asker's share still decides how wide.** Each member takes its own admission slot inside
    // [`tiles_json`], under the batch's `client`, exactly as a single-tile read does — so a batch
    // never holds more than that client's share, and never more than the global cap. A worker whose
    // member is refused `503` while another worker is still running hands the address back and
    // stops: the refusal said this batch is already as wide as the share allows, and one worker
    // fewer is the answer to that, not a per-address 503 for every member it would have tried. Only
    // the LAST worker records a 503, because then nobody in this batch holds a slot and the refusal
    // is genuinely the route's contention, which the caller re-asks for.
    let answer = |a: &BatchAddr| -> (Value, u16) {
        let mut params = shared.clone();
        params.push(("level_f".into(), a.level_f.to_string()));
        params.push(("level_t".into(), a.level_t.to_string()));
        params.push(("f_index".into(), a.f_index.to_string()));
        params.push(("t_index".into(), a.t_index.to_string()));
        let address = json!({
            "level_f": a.level_f,
            "level_t": a.level_t,
            "f_index": a.f_index,
            "t_index": a.t_index,
            "spelling": a.spelling,
        });
        match tiles_json(state, &params) {
            // Verbatim. The single-tile body IS the per-address answer; nothing is dropped,
            // merged or re-keyed on the way into the array.
            Ok(v) => (json!({ "address": address, "status": 200, "tile": v }), 200),
            // A per-address refusal with its own status — the whole reason this is an array of
            // entries and not an array of tiles.
            Err(e) => (
                json!({ "address": address, "status": e.status, "error": e.message }),
                e.status,
            ),
        }
    };
    // Addresses are taken lowest-first, so what has been answered is a prefix plus whatever is
    // still running; workers stop taking once the answered bytes cross the cap. The cut below is
    // made in address order, so truncation is as deterministic as it was sequentially.
    let answered: Vec<Mutex<Option<(Value, usize)>>> =
        addrs.iter().map(|_| Mutex::new(None)).collect();
    let todo: Mutex<std::collections::VecDeque<usize>> = Mutex::new((0..addrs.len()).collect());
    let workers = addrs.len().clamp(1, TILE_MAX_IN_FLIGHT);
    let active = AtomicUsize::new(workers);
    let spent = AtomicUsize::new(0);
    let work = || {
        loop {
            if spent.load(Ordering::Acquire) >= max_bytes {
                break;
            }
            let Some(i) = todo.lock().expect("batch queue").pop_front() else {
                break;
            };
            let (entry, status) = answer(&addrs[i]);
            if status == 503 {
                // Give the address back and retire, unless this is the last worker standing.
                let last = active
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                        (n > 1).then(|| n - 1)
                    })
                    .is_err();
                if !last {
                    todo.lock().expect("batch queue").push_front(i);
                    return;
                }
            }
            // The size cap is charged on what this answer will actually put on the wire.
            let n = serde_json::to_vec(&entry).map(|b| b.len()).unwrap_or(0);
            spent.fetch_add(n, Ordering::AcqRel);
            *answered[i].lock().expect("batch slot") = Some((entry, n));
        }
        active.fetch_sub(1, Ordering::AcqRel);
    };
    if workers == 1 {
        work();
    } else {
        std::thread::scope(|s| {
            for _ in 1..workers {
                s.spawn(work);
            }
            work();
        });
    }
    // A worker can retire on a 503 in the instant the last one found the queue empty, leaving its
    // address handed back with nobody to take it. Answer those here, in order, with whatever the
    // route says now — a 503 included — so no address is ever left out without a status.
    let left: Vec<usize> = todo.lock().expect("batch queue").drain(..).collect();
    for i in left {
        if spent.load(Ordering::Acquire) >= max_bytes {
            break;
        }
        let (entry, _) = answer(&addrs[i]);
        let n = serde_json::to_vec(&entry).map(|b| b.len()).unwrap_or(0);
        spent.fetch_add(n, Ordering::AcqRel);
        *answered[i].lock().expect("batch slot") = Some((entry, n));
    }

    let mut tiles = Vec::with_capacity(addrs.len());
    let mut bytes = 0usize;
    let mut truncated = false;
    let mut remaining: Vec<String> = Vec::new();
    for (i, (a, slot)) in addrs.iter().zip(answered).enumerate() {
        let got = slot.into_inner().expect("batch slot");
        match got {
            Some((entry, n)) if !truncated => {
                bytes += n;
                tiles.push(entry);
                // `i > 0`: one oversized address must still be answerable, or it is unfetchable
                // forever.
                if bytes >= max_bytes && i + 1 < addrs.len() {
                    truncated = true;
                }
            }
            // Not reached, or reached by a worker racing past the cap: either way it is not in
            // this answer, and it is named so the follow-up request is a copy.
            _ => {
                truncated = true;
                remaining.push(a.spelling.clone());
            }
        }
    }

    Ok(json!({
        "requested": addrs.len(),
        "returned": tiles.len(),
        "truncated": truncated,
        // Exactly the addresses that were not answered, in the spelling they were asked in, so the
        // follow-up request is a copy rather than a re-derivation.
        "remaining": remaining,
        "limits": {
            "max_addresses": TILES_BATCH_MAX_ADDRESSES,
            "max_response_bytes": max_bytes,
            "over_addresses": "refused (400), naming the cap",
            "over_bytes": "truncated, with every unanswered address listed in `remaining`",
        },
        "tiles": tiles,
        "statement": "each entry's `tile` is exactly what GET /api/tiles answers for that address \
            alone — same key, same level pair, same coverage plane, same cost. Batching changes \
            how many requests a viewport costs, never what a tile says. A tile with no data is \
            still answered from the coverage map without reaching the generation path.",
    }))
}

/// One address in a batch, plus the exact text it was asked in.
struct BatchAddr {
    level_f: u32,
    level_t: u32,
    f_index: i64,
    t_index: i64,
    spelling: String,
}

/// `<level_f>.<level_t>.<f_index>.<t_index>`, comma-separated.
///
/// Dotted and comma-separated rather than repeated query parameters: it keeps a sixty-four-address
/// viewport around a kilobyte, well inside the 16 KiB request-head limit, and it keeps ONE
/// parameter to validate. A malformed address refuses the whole request rather than being skipped
/// — a silently-dropped address is a tile the canvas would leave pending forever with nothing
/// saying why.
fn parse_batch_addresses(raw: &str) -> Result<Vec<BatchAddr>, ApiError> {
    let mut out = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let f: Vec<&str> = part.split('.').collect();
        let malformed = || {
            bad(&format!(
                "address {part:?} is not <level_f>.<level_t>.<f_index>.<t_index>: a batch refuses \
                 a malformed address rather than dropping it, because a dropped address is a tile \
                 left pending with nothing saying why"
            ))
        };
        if f.len() != 4 {
            return Err(malformed());
        }
        out.push(BatchAddr {
            level_f: f[0].parse().map_err(|_| malformed())?,
            level_t: f[1].parse().map_err(|_| malformed())?,
            f_index: f[2].parse().map_err(|_| malformed())?,
            t_index: f[3].parse().map_err(|_| malformed())?,
            spelling: part.to_owned(),
        });
    }
    if out.is_empty() {
        return Err(bad(
            "addresses= named no address: a batch with no addresses is not a cheaper spelling of \
             anything",
        ));
    }
    Ok(out)
}

/// `GET /api/tiles/events` — the coarse-zoom aggregate form (`docs/16` §5.3).
///
/// A tile carries **no emitters**: identity gating is per-caller and a tile is not, and a sealed
/// tile is immutable while an emitter set changes on every ledger append. So the highlight layer is
/// a separate query on the same address, and at a coarse zoom it is a **count per cell** — never a
/// box inflated to be visible, which would fabricate a timespan.
///
/// **One count per event, at the cell holding its start.** An event is a presence interval with its
/// own extent (CLAUDE.md invariant 1); counting it once per row it crosses would inflate a long
/// emission into a busy band. The rule is on the wire rather than left to be inferred.
pub fn tile_events_json(state: &ApiState, q: &Params) -> Result<Value, ApiError> {
    const ALLOWED: [&str; 9] = [
        "device", "scheme", "level_f", "level_t", "f_index", "t_index", "cells", "state", "token",
    ];
    if let Some((k, _)) = q.iter().find(|(k, _)| !ALLOWED.contains(&k.as_str())) {
        return Err(bad(&format!("unknown parameter {k:?}")));
    }
    let key = with_tile_history(state, tile_store(state, q), |p| parse_key(p.geometry(), q))?;
    let repo = state
        .inventory
        .as_ref()
        .ok_or_else(|| ApiError::new(404, "no inventory on this server"))?;
    let repo = repo
        .lock()
        .map_err(|_| ApiError::new(500, "inventory poisoned"))?;
    let window = key.window();
    let cells = key.cells;
    let mut counts = vec![0u32; cells * cells];
    let mut query = crate::query::parse_inventory_query(&[
        ("f_lo".into(), key.region.freq.lo_hz.to_string()),
        ("f_hi".into(), key.region.freq.hi_hz.to_string()),
        ("t0".into(), (key.region.t0_ns as f64 / 1e9).to_string()),
        ("t1".into(), (key.region.t1_ns as f64 / 1e9).to_string()),
    ])?;
    query.limit = crate::events::MAX_EVENT_EMITTERS;
    query.offset = 0;
    let page = repo
        .query_inventory(&query)
        .map_err(|_| ApiError::new(500, "event query failed"))?;
    let mut total = 0u64;
    let mut placed = 0u64;
    // T-591: the same tune history `/api/events` and `/api/inventory` read, so a count on this
    // tile is a count of the same events those surfaces list. It used to be
    // `IdleGap::conservative` here, which is a *different* interval decomposition of the same
    // ledger — the coarse view could disagree with the list it zooms into about how many events
    // there were.
    let coverage = crate::coverage::ObservedCoverage::of(state, window);
    for entry in &page.entries {
        let track = coverage
            .track(
                &repo,
                entry.emitter.id,
                entry.emitter.freq(),
                window,
                window.end,
            )
            .map_err(|_| ApiError::new(500, "event query failed"))?;
        for i in track.intervals.iter().filter(|i| i.time.overlaps(&window)) {
            total += 1;
            let t = (i.time.start.as_unix_nanos() - key.region.t0_ns) / key.t_cell_ns;
            // An interval with no measured centre has no column: it is counted in `total` and not
            // placed, exactly as /api/events declines to invent a timespan it did not measure.
            let Some(centre) = i.f_center_hz else {
                continue;
            };
            let f = ((centre - key.region.freq.lo_hz) / key.f_cell_hz).floor();
            // An event that began before this tile, or off its band, is counted in `total` and not
            // placed: clamping it to the edge would put activity in a cell it never occupied.
            if t < 0 || t >= cells as i64 || !(f >= 0.0 && f < cells as f64) {
                continue;
            }
            counts[t as usize * cells + f as usize] += 1;
            placed += 1;
        }
    }
    Ok(json!({
        "key": key_json(&key),
        "extent": {
            "f_lo_hz": key.region.freq.lo_hz,
            "f_hi_hz": key.region.freq.hi_hz,
            "f_cell_hz": key.f_cell_hz,
            "t0_s": key.region.t0_ns as f64 / 1e9,
            "t1_s": key.region.t1_ns as f64 / 1e9,
            "t_cell_s": key.t_cell_ns as f64 / 1e9,
            "nt": cells,
            "nf": cells,
        },
        // Row-major on exactly the tile's axes, so a client indexes this with the tile's index.
        "counts": counts,
        "total": total,
        "placed": placed,
        "emitters_scanned": page.entries.len(),
        "emitters_truncated": page.next_offset.is_some(),
        "rule": "ONE COUNT PER EVENT, placed at the cell holding its START. An event is a presence \
            interval with its own extent; counting it once per row it crosses would inflate a long \
            emission into a busy band, and scaling a sub-cell burst up to be visible would \
            fabricate a timespan (docs/16 §5.3). An event beginning before this tile, or centred \
            off its band, is counted in `total` and not placed — `placed` is what the grid holds.",
        "not_a_tile_channel": "counts change with every append to the observation ledger, so they \
            are computed on demand and never sealed into a tile: a tile stays immutable and \
            cacheable, which is the property its storage was bought for.",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_store::history::{PyramidConfig, ViewLattice};

    fn geom() -> Geometry {
        PyramidConfig::default().geometry().unwrap()
    }

    fn params(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    }

    /// The view lattice's floor is the STORE's level-0 cell, not `docs/16` §6.2's 100 kHz × 128 s
    /// (T-437 finding F1): a 128 s finest time cell puts the whole 120 s IQ retention inside one
    /// cell, so `level_t` pins at 0 and the de-welding buys nothing on the axis it exists for.
    #[test]
    fn the_view_lattices_floor_is_the_stores_own_level_zero_cell() {
        let g = geom();
        let l = TileLattice::view(&g);
        assert_eq!(l.f_cells_hz[0], g.levels[0].f_cell_hz);
        assert_eq!(l.t_cells_ns[0], g.levels[0].t_cell_ns);
        assert_eq!(l.t_cells_ns[0], 1_000_000_000, "one second, not 128");
        // Each axis doubles independently, and reaches docs/16 §6.2's widest and tallest tile.
        for w in l.f_cells_hz.windows(2) {
            assert_eq!(w[1], w[0] * 2.0);
        }
        for w in l.t_cells_ns.windows(2) {
            assert_eq!(w[1], w[0] * 2);
        }
        let widest = l.f_cells_hz.last().unwrap() * TILE_CELLS as f64;
        assert!(widest >= VIEW_MAX_TILE_HZ, "{widest}");
        let tallest = l.t_cells_ns.last().unwrap() * TILE_CELLS as i64;
        assert!(tallest >= VIEW_MAX_TILE_NS, "{tallest}");
    }

    /// **T-505's whole measurement, in one test: reach is a property of the STORE's coarsest
    /// cell, and no ceiling, depth or `cells` recovers it.**
    ///
    /// The ceiling is a bound on level *index* and [`servable`] is blind to absolute size, so a
    /// floor N doublings finer costs exactly 2^N of addressable tile span. T-484 paid that and the
    /// user's tile map went dark. This asserts the three geometries side by side, so neither the
    /// defect nor its repair can move unnoticed — and in particular so that the overview tier
    /// cannot quietly be re-anchored on the view pyramid, which is the one change that would
    /// reintroduce T-484 with every suite still green.
    #[test]
    fn the_overview_tier_reaches_past_the_whole_surface_whatever_the_view_floor_is() {
        use std::time::Duration;

        fn open(tag: &str, cfg: PyramidConfig) -> hk_store::Pyramid {
            let p = std::env::temp_dir().join(format!(
                "hk-api-t505-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            hk_store::Pyramid::open(&p, cfg).unwrap()
        }
        /// `(ceiling, coarsest addressable tile in Hz, in ns)` for a lattice over its own store.
        fn reach(p: &hk_store::Pyramid, lat: &TileLattice) -> ((usize, usize), f64, i64) {
            let c = readable_ceiling(p, lat);
            (
                c,
                lat.f_cells_hz[c.0] * TILE_CELLS as f64,
                lat.t_cells_ns[c.1] * TILE_CELLS as i64,
            )
        }
        /// A view pyramid with the shipped 4 × 4 shape and the given node-(0, 0) cell.
        fn view_pyramid(tag: &str, f0_hz: f64, t0: Duration) -> hk_store::Pyramid {
            open(
                tag,
                PyramidConfig {
                    f_cells_per_block: 1024,
                    ..PyramidConfig::view_lattice(ViewLattice {
                        scheme: 2,
                        f_cell_hz: f0_hz,
                        t_cell: t0,
                        cells_per_block: 64,
                        f_levels: 4,
                        t_levels: 4,
                    })
                },
            )
        }

        // The shipped view floor: scheme 1's own level-0 cell. 819.2 MHz x 512 s of reach.
        let shipped = view_pyramid("shipped", 6250.0, Duration::from_secs(1));
        let (c, f_hz, t_ns) = reach(&shipped, &TileLattice::view(shipped.geometry()));
        assert_eq!(c, (9, 1));
        assert_eq!((f_hz, t_ns), (819_200_000.0, 512_000_000_000));

        // T-484's floor: the display STFT's own bin and row at 2.4 Msps, eight doublings finer in
        // frequency and ~4.6 in time. THE CEILING DOES NOT MOVE, so the reach collapses 133x in
        // area, and a 6 GHz x 30 min minimap needs 7031 addresses instead of 32.
        let fine = view_pyramid("t484", 585.937_5, Duration::from_nanos(40_106_667));
        let (c_fine, f_fine, t_fine) = reach(&fine, &TileLattice::view(fine.geometry()));
        assert_eq!(
            c_fine, c,
            "the ceiling is an INDEX bound and must not move with the floor"
        );
        assert_eq!(f_fine, 76_800_000.0);
        assert_eq!(t_fine, 20_534_613_504);
        assert!(
            (f_hz / f_fine) * (t_ns as f64 / t_fine as f64) > 130.0,
            "the fine floor should cost ~133x of tile AREA",
        );

        // The overview tier, anchored on the spectrum-history pyramid. Its geometry does not move
        // when the view floor does, so this row is the SAME whichever of the two above is open.
        let main = open("main", PyramidConfig::default());
        let over = TileLattice::overview(main.geometry());
        assert_eq!(over.name, "overview");
        let (c_over, f_over, t_over) = reach(&main, &over);
        assert_eq!(c_over, (11, 14));
        assert_eq!(
            f_over, 3_276_800_000.0,
            "wider than the 1 MHz-6 GHz surface"
        );
        assert!(
            t_over as f64 / 1e9 > 30.0 * 86_400.0,
            "taller than any retention window ({t_over} ns)",
        );
        // The property that makes the tiers real rather than labels: the overview tier's coarsest
        // tile is wider than the whole device range AND longer than a month, at BOTH view floors.
        // §6.2's V7: the whole 1 MHz-6 GHz range in **two** tiles (`VIEW_MAX_TILE_HZ` is 3 GHz),
        // against 79 at the fine floor.
        assert!((6e9f64 / f_over).ceil() == 2.0 && (6e9 / f_fine).ceil() == 79.0);
        assert!(f_over > f_fine * 40.0);
    }

    /// `scheme=overview` is a third spelling, and it resolves to the spectrum-history pyramid
    /// whether or not a view pyramid is open — the two tiers must not be able to become one.
    #[test]
    fn the_overview_scheme_names_its_own_lattice_and_the_spectrum_history_store() {
        let g = geom();
        let key = parse_key(
            &g,
            &params(&[
                ("scheme", "overview"),
                ("level_f", "10"),
                ("level_t", "4"),
                ("f_index", "0"),
                ("t_index", "0"),
            ]),
        )
        .unwrap();
        assert_eq!(key.lattice.name, "overview");
        assert!(!key.lattice.from_store, "it is de-welded, not the ladder");
        assert_eq!(key.f_cell_hz, 6250.0 * 2f64.powi(10));
        assert_eq!(key.t_cell_ns, 16_000_000_000);
        // A `view` address of the same indices names the same cells on a server with no view
        // pyramid open, and a DIFFERENT scheme name — so the two never share a cache key.
        let view = parse_key(
            &g,
            &params(&[
                ("level_f", "10"),
                ("level_t", "4"),
                ("f_index", "0"),
                ("t_index", "0"),
            ]),
        )
        .unwrap();
        assert_eq!(view.lattice.name, "view");
        assert_ne!(view.lattice.name, key.lattice.name);

        // The store choice, with and without a view pyramid open.
        let empty = ApiState::default();
        assert_eq!(
            tile_store(&empty, &params(&[("scheme", "overview")])),
            TileStore::Main,
        );
        let dir = std::env::temp_dir().join(format!(
            "hk-api-t505-store-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let state = ApiState {
            view_history: Some(Arc::new(std::sync::Mutex::new(
                hk_store::Pyramid::open(
                    &dir,
                    PyramidConfig {
                        f_cells_per_block: 1024,
                        ..PyramidConfig::view_lattice(ViewLattice::default())
                    },
                )
                .unwrap(),
            ))),
            ..ApiState::default()
        };
        assert_eq!(
            tile_store(&state, &params(&[("scheme", "overview")])),
            TileStore::Main,
            "the overview tier must never be answered by the view pyramid: that is T-484",
        );
        assert_eq!(tile_store(&state, &params(&[])), TileStore::View);
        assert_eq!(
            tile_store(&state, &params(&[("scheme", "view")])),
            TileStore::View,
        );
    }

    /// A welded ladder is the **diagonal** of its own lattice, and off it there is no node
    /// (T-434). The route says so rather than snapping to a level whose time cell is a day.
    #[test]
    fn a_welded_ladders_off_diagonal_node_is_refused_and_never_snapped() {
        let g = geom();
        // Scheme 1's diagonal: (3, 3) is level 3 — 50 kHz × 3600 s.
        let on = parse_key(
            &g,
            &params(&[
                ("scheme", "1"),
                ("level_f", "3"),
                ("level_t", "3"),
                ("f_index", "0"),
                ("t_index", "0"),
            ]),
        )
        .unwrap();
        assert_eq!(on.store_node, Some(3));
        assert_eq!(on.f_cell_hz, 50_000.0);
        assert_eq!(on.t_cell_ns, 3_600_000_000_000);
        // Off it: fine frequency, coarse time. A ladder cannot express it.
        let err = parse_key(
            &g,
            &params(&[
                ("scheme", "1"),
                ("level_f", "0"),
                ("level_t", "3"),
                ("f_index", "0"),
                ("t_index", "0"),
            ]),
        )
        .unwrap_err();
        assert_eq!(err.status, 404, "{}", err.message);
        assert!(err.message.contains("diagonal"), "{}", err.message);
        // Past the end of an axis is the other kind of "no such node", and names both extents.
        let err = parse_key(
            &g,
            &params(&[
                ("scheme", "1"),
                ("level_f", "99"),
                ("level_t", "0"),
                ("f_index", "0"),
                ("t_index", "0"),
            ]),
        )
        .unwrap_err();
        assert_eq!(err.status, 404, "{}", err.message);
        assert!(err.message.contains("level_f 0..5"), "{}", err.message);
    }

    /// The de-welding, at the addressing layer: **changing one axis's level moves only that axis's
    /// cell.** This is the structural reason T-437's F2 cannot happen here — there, tightening the
    /// *frequency* budget 1.5× cost 34× of *time* resolution.
    #[test]
    fn changing_one_axis_level_never_moves_the_other_axis_cell() {
        let g = geom();
        let key = |lf: &str, lt: &str| {
            parse_key(
                &g,
                &params(&[
                    ("level_f", lf),
                    ("level_t", lt),
                    ("f_index", "0"),
                    ("t_index", "0"),
                ]),
            )
            .unwrap()
        };
        let base = key("0", "0");
        for lf in ["1", "2", "5", "9"] {
            let k = key(lf, "0");
            assert_eq!(
                k.t_cell_ns, base.t_cell_ns,
                "level_f {lf} moved the TIME cell"
            );
            assert!(k.f_cell_hz > base.f_cell_hz);
        }
        for lt in ["1", "2", "5", "9"] {
            let k = key("0", lt);
            assert_eq!(
                k.f_cell_hz, base.f_cell_hz,
                "level_t {lt} moved the FREQUENCY cell"
            );
            assert!(k.t_cell_ns > base.t_cell_ns);
        }
    }

    /// Candidates are ordered by **cell area**, not by level index — T-434's warning made a test.
    /// On a real de-welded lattice index order is not a coarseness order, and ordering by it would
    /// answer a coarse time question with a fine time cell.
    #[test]
    fn candidate_levels_are_ordered_by_cell_area_and_index_order_would_be_wrong() {
        let lattice_geom = PyramidConfig::view_lattice(ViewLattice {
            scheme: 7,
            f_cell_hz: 6250.0,
            t_cell: std::time::Duration::from_secs(1),
            cells_per_block: 256,
            f_levels: 4,
            t_levels: 4,
        })
        .geometry()
        .unwrap();
        // Node (1, 0) has a LOWER index than (0, 3) in nothing — index is f*t_levels + t, so
        // (1, 0) is index 4 and (0, 3) is index 3: (0, 3) comes first by index while being
        // coarser in time by 8x. Index order is not coarseness order.
        let index = |lf: usize, lt: usize| lf * 4 + lt;
        let (i_10, i_03) = (index(1, 0), index(0, 3));
        assert!(i_03 < i_10, "index order puts the time-coarse node first");
        assert!(
            lattice_geom.levels[i_03].t_cell_ns > lattice_geom.levels[i_10].t_cell_ns,
            "…and it is genuinely coarser in time"
        );
        let key = parse_key(
            &lattice_geom,
            &params(&[
                ("level_f", "1"),
                ("level_t", "1"),
                ("f_index", "0"),
                ("t_index", "0"),
            ]),
        )
        .unwrap();
        let got = read_affordable_levels(&lattice_geom, &key);
        let area =
            |l: usize| lattice_geom.levels[l].f_cell_hz * lattice_geom.levels[l].t_cell_ns as f64;
        for w in got.windows(2) {
            assert!(
                area(w[0]) <= area(w[1]),
                "candidates must be finest-first by area: {got:?}"
            );
        }
        // Finest first, so level 0 leads whenever it is affordable.
        assert_eq!(got.first(), Some(&0), "{got:?}");
    }

    /// The work budget is a **work** budget: it excludes levels that cost too much to read and
    /// never reaches for a coarser one to save cells.
    #[test]
    fn the_work_budget_excludes_unaffordable_levels_and_keeps_the_finest_that_fits() {
        let g = geom();
        // A wide, tall tile: 6.4 MHz x 65 536 s at (level_f 7, level_t 8).
        let key = parse_key(
            &g,
            &params(&[
                ("level_f", "7"),
                ("level_t", "8"),
                ("f_index", "0"),
                ("t_index", "0"),
            ]),
        )
        .unwrap();
        let got = read_affordable_levels(&g, &key);
        assert!(!got.is_empty());
        // Level 0 (6.25 kHz x 1 s) over that extent is 128 x 65 536 source cells and must be out.
        let (nt, nf) = dims(&g, 0, &key.region);
        assert!(nt * nf > TILE_MAX_TOTAL_SOURCE_CELLS as f64);
        assert!(!got.contains(&0), "{got:?}");
        // Whatever leads is the finest of those that fit.
        let lead = got[0];
        for &l in &got {
            let area = |l: usize| g.levels[l].f_cell_hz * g.levels[l].t_cell_ns as f64;
            assert!(area(lead) <= area(l));
        }
    }

    /// The in-flight cap is a real cap: the N+1th reader is refused, and a released slot is reusable.
    #[test]
    fn the_in_flight_cap_refuses_over_the_limit_and_releases_on_drop() {
        let a = Arc::new(TileAdmission::default());
        let held: Vec<TileSlot> = (0..TILE_MAX_IN_FLIGHT)
            .map(|_| a.acquire("one").expect("under the cap"))
            .collect();
        assert_eq!(held.last().unwrap().in_flight(), TILE_MAX_IN_FLIGHT);
        let d = a.acquire("one").expect_err("the cap must bind");
        let err = too_many_in_flight(d);
        assert_eq!(err.status, 503);
        assert!(
            err.message.contains(&TILE_MAX_IN_FLIGHT.to_string()),
            "the body must NAME the cap: {}",
            err.message
        );
        drop(held);
        assert_eq!(a.in_flight(), 0);
        assert!(a.acquire("one").is_ok());
    }

    /// **The whole point of T-630**: one client cannot hold the whole cap once a second client is
    /// asking. The share is `ceil(cap / clients)`, and it is computed from clients that are
    /// *asking*, including one whose only request so far was refused.
    #[test]
    fn a_second_client_takes_a_share_of_the_cap_from_the_first() {
        let a = Arc::new(TileAdmission::default());
        // One client alone gets the whole cap — the share costs nothing when nobody else is here.
        let mut first: Vec<TileSlot> = (0..TILE_MAX_IN_FLIGHT)
            .map(|_| a.acquire("first").expect("alone, under the cap"))
            .collect();
        assert_eq!(first[0].share().share, TILE_MAX_IN_FLIGHT);
        assert_eq!(first[0].share().clients, 1);

        // The second client's FIRST request is refused — four reads are genuinely out — but it is
        // that refused request which registers it, so the first client's share is halved from here.
        let d = a.acquire("second").expect_err("four are out");
        assert_eq!(d.clients, 2);
        assert_eq!(d.share, TILE_MAX_IN_FLIGHT.div_ceil(2));

        // The first client re-asking the instant a slot frees is exactly the behaviour that starved
        // the second one. It is now refused above its share, whatever it does.
        first.pop();
        let d = a.acquire("first").expect_err("over its share");
        assert_eq!(d.share, TILE_MAX_IN_FLIGHT.div_ceil(2));
        assert!(d.in_flight < TILE_MAX_IN_FLIGHT, "a slot WAS free: {d:?}");

        // And the slot it could not take is the second client's.
        let s = a.acquire("second").expect("its share is free");
        assert_eq!(s.client(), "second");
        assert_eq!(s.held(), 1);
    }

    /// **The bootstrap reserve.** The share alone still lets the drawn clients fill the cap
    /// between them, and then a newcomer's first request — the one it cannot start without — waits
    /// on somebody's tile read. So while a client that has never been served a tile is asking, the
    /// already-drawn clients are admitted only up to `cap - 1`.
    #[test]
    fn a_client_with_nothing_on_screen_yet_has_a_slot_held_for_it() {
        let a = Arc::new(TileAdmission::default());
        // Two clients, both drawn, holding the whole cap between them and inside their shares.
        for id in ["a", "b"] {
            a.acquire(id).unwrap().mark_served();
        }
        let mut a_held: Vec<TileSlot> = ["a", "a", "b", "b"]
            .iter()
            .map(|c| a.acquire(c).unwrap())
            .collect();
        assert_eq!(a.in_flight(), TILE_MAX_IN_FLIGHT);

        // The newcomer's first request meets a genuinely full route and is refused — but it is now
        // known, and it is known to have nothing on screen.
        let d = a.acquire("new").expect_err("four are really out");
        assert_eq!(d.clients, 3);
        assert_eq!(
            d.reserved, 0,
            "the reserve is never held against the client it is for"
        );

        // A slot frees. A drawn client re-asking the instant that happens is the exact behaviour
        // that starved the newcomer, and it is **the reserve** that refuses it here: "a" is inside
        // its share of two.
        drop(a_held.remove(0));
        let d = a
            .acquire("a")
            .expect_err("the free slot is held for the newcomer");
        assert_eq!((d.share, d.reserved), (2, 1), "{d:?}");
        assert_eq!(
            d.in_flight,
            TILE_MAX_IN_FLIGHT - 1,
            "a slot WAS free: {d:?}"
        );

        // It is the newcomer's, and once it has been served the reserve is gone.
        let first_paint = a.acquire("new").expect("the reserved slot");
        first_paint.mark_served();
        drop(first_paint);
        let back = a
            .acquire("a")
            .expect("the reserve is disarmed once the newcomer is drawn");
        assert_eq!(back.share().reserved, 0);
    }

    /// A client that disappears without saying so must not keep its share forever (the leak shape
    /// T-454 paid for). Its slots are released on drop, and its entry is swept once it is idle.
    #[test]
    fn a_client_that_vanishes_gives_its_share_back() {
        let a = Arc::new(TileAdmission::default());
        let gone = a.acquire("gone").unwrap();
        let mine = a.acquire("mine").unwrap();
        assert_eq!(mine.share().clients, 2);
        drop(gone);
        // Still two, because "gone" asked a moment ago — it is idleness that forgets a client, not
        // an empty slot count, or a client between requests would lose its share mid-pan.
        assert_eq!(a.acquire("mine").unwrap().share().clients, 2);
        // Age it out by hand: the table is a cache of who is asking now.
        {
            let mut t = a.clients.lock().unwrap();
            for c in t.iter_mut() {
                if c.id == "gone" {
                    c.last_seen -= TILE_CLIENT_IDLE * 2;
                }
            }
        }
        assert_eq!(
            a.acquire("mine").unwrap().share().clients,
            1,
            "the share came back"
        );
    }

    /// Requests that declare no client share one bucket, so the route behaves for `curl` and the
    /// CLI exactly as it did before T-630.
    #[test]
    fn undeclared_clients_share_the_anonymous_bucket() {
        assert_eq!(client_id(&params(&[])), ANONYMOUS_CLIENT);
        assert_eq!(client_id(&params(&[("client", "a-b.c:1")])), "a-b.c:1");
        assert_eq!(
            client_id(&params(&[("client", "no spaces")])),
            ANONYMOUS_CLIENT
        );
        assert_eq!(
            client_id(&params(&[("client", &"x".repeat(65))])),
            ANONYMOUS_CLIENT
        );
        let a = Arc::new(TileAdmission::default());
        let s = a.acquire(&client_id(&params(&[]))).unwrap();
        let t = a.acquire(&client_id(&params(&[("token", "k")]))).unwrap();
        assert_eq!(s.client(), t.client());
        assert_eq!(t.share().clients, 1);
    }

    // ---- store-backed reads ----------------------------------------------------------------

    /// Tile edge used by the store-backed tests: small enough to keep them quick, large enough
    /// that a fold is a real fold.
    const N: usize = 64;
    /// Tile index on the time axis, chosen so the tile's extent is epoch-aligned by construction.
    const T_INDEX: i64 = 27_958_762;
    /// Tile index on the frequency axis at `level_f = 0`: **even**, so the `level_f + 1` tile
    /// covering it starts at the same edge.
    const F_INDEX: i64 = 1_116;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "hk-api-tiles-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A pyramid holding `secs` seconds of frames over 400 kHz from the tile's own low edge, with
    /// one carrier, so an observed cell is observed because a frame landed there.
    fn state_with_history(dir: &std::path::Path, secs: i64) -> (ApiState, i64, f64) {
        let mut p = hk_store::Pyramid::open(dir, PyramidConfig::default()).unwrap();
        let g = p.geometry().clone();
        let t_cell = g.levels[0].t_cell_ns;
        let f_cell = g.levels[0].f_cell_hz;
        let t0 = T_INDEX * t_cell * N as i64;
        let f_lo = F_INDEX as f64 * f_cell * N as f64;
        const NB: usize = 128;
        let bin_hz = f_cell * N as f64 / NB as f64;
        for k in 0..secs {
            let mut psd = [1e-12f32; NB];
            psd[40] = 1e-6;
            p.ingest(&hk_store::history::FrameInput::new(
                Timestamp::from_unix_nanos(t0 + k * t_cell),
                t_cell,
                f_lo,
                bin_hz,
                hk_model::PowerUnit::Dbfs,
                &psd,
            ))
            .unwrap();
        }
        let state = ApiState {
            history: Some(Arc::new(std::sync::Mutex::new(p))),
            ..ApiState::default()
        };
        (state, t0, f_lo)
    }

    fn read_at(
        state: &ApiState,
        level_f: &str,
        level_t: &str,
        f_index: i64,
        t_index: i64,
    ) -> (TileKey, TileRead) {
        let g = crate::http::with_history(state, |p| Ok(p.geometry().clone())).unwrap();
        let key = parse_key(
            &g,
            &params(&[
                ("level_f", level_f),
                ("level_t", level_t),
                ("f_index", &f_index.to_string()),
                ("t_index", &t_index.to_string()),
                ("cells", &N.to_string()),
            ]),
        )
        .unwrap();
        let r = tile_read(state, TileStore::Main, &key).unwrap();
        (key, r)
    }

    fn read(state: &ApiState, level_f: &str, level_t: &str, f_index: i64) -> (TileKey, TileRead) {
        read_at(state, level_f, level_t, f_index, T_INDEX)
    }

    /// The tile's grid is **always** its own `cells × cells` on its own extent, whatever level
    /// answered — and the level that answered is reported, not the one the address implies (T-426).
    #[test]
    fn a_tile_is_folded_onto_its_own_cells_and_names_the_level_that_answered() {
        let dir = temp_dir("fold");
        let (state, t0, f_lo) = state_with_history(&dir, N as i64);
        let (key, r) = read(&state, "0", "0", F_INDEX);
        assert_eq!((r.grid.nt, r.grid.nf), (N, N));
        assert_eq!(r.grid.t0_ns, t0);
        assert_eq!(r.grid.f_lo_hz, f_lo);
        assert_eq!(
            key.region.t1_ns - key.region.t0_ns,
            N as i64 * 1_000_000_000
        );
        // Level 0 is the finest affordable level and it holds the data, so it answers and the
        // address's node is exactly it.
        assert_eq!(r.level, 0, "tried {:?}", r.tried);
        assert_eq!(key.store_node, Some(0));
        assert_eq!(r.tried, vec![0], "no walk was needed: {:?}", r.tried);
        assert!(r.grid.observed_cells > 0, "the fixture's frames must show");
        // Exactly one chunk: N x N source cells is far inside one lock hold's budget.
        assert_eq!(r.chunks, 1);
        eprintln!(
            "T-438 tile (0,0): observed {}/{} cells, {} source cells, {} chunk(s)",
            r.grid.observed_cells,
            N * N,
            r.source_cells,
            r.chunks
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The F2 guard.** T-437 measured `/api/history` losing 34× of *time* resolution and greying
    /// a third of the window when only the *frequency* budget was tightened 1.5×. The same
    /// experiment here — same window, same data, only the frequency address coarsened one step —
    /// must cost neither: the time cell is identical, and every time row the fine address shows as
    /// observed is observed at the coarse one too.
    #[test]
    fn coarsening_the_frequency_address_costs_no_time_resolution_and_greys_nothing() {
        let dir = temp_dir("f2");
        let (state, _, _) = state_with_history(&dir, N as i64);
        let (fine_key, fine) = read(&state, "0", "0", F_INDEX);
        let (coarse_key, coarse) = read(&state, "1", "0", F_INDEX / 2);
        // The two tiles start at the same frequency edge and span the same window.
        assert_eq!(fine_key.region.freq.lo_hz, coarse_key.region.freq.lo_hz);
        assert_eq!(fine_key.region.t0_ns, coarse_key.region.t0_ns);
        assert_eq!(fine_key.region.t1_ns, coarse_key.region.t1_ns);
        // F2's exact failure: the time axis moved when only the frequency axis was asked to.
        assert_eq!(
            fine_key.t_cell_ns, coarse_key.t_cell_ns,
            "coarsening level_f must not touch the time cell"
        );
        assert_eq!(
            fine.grid.t_cell_ns, coarse.grid.t_cell_ns,
            "…nor the served grid's"
        );
        // F2's other half: a third of the window went grey. Every row the fine tile observed must
        // be observed in the coarse tile's lower half, which covers exactly the fine tile's band.
        let observed_rows = |g: &Overview, lo: usize, hi: usize| -> Vec<bool> {
            (0..N)
                .map(|t| (lo..hi).any(|f| g.cells[t * N + f].observed()))
                .collect()
        };
        let fine_rows = observed_rows(&fine.grid, 0, N);
        let coarse_rows = observed_rows(&coarse.grid, 0, N / 2);
        assert!(
            fine_rows.iter().any(|&b| b),
            "the fixture must observe rows"
        );
        for (t, (&a, &b)) in fine_rows.iter().zip(&coarse_rows).enumerate() {
            assert!(
                !a || b,
                "row {t}: the fine address observed it and the coarse one greyed it"
            );
        }
        eprintln!(
            "T-438 F2 guard: fine observed {} rows, coarse {} rows, t_cell {} s both",
            fine_rows.iter().filter(|b| **b).count(),
            coarse_rows.iter().filter(|b| **b).count(),
            fine.grid.t_cell_ns / 1e9
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A tile over a window nothing observed is grey **and says which levels it consulted** — the
    /// walk happened and found nothing, which is a different statement from not having walked.
    #[test]
    fn an_unobserved_tile_is_grey_and_reports_every_level_it_consulted() {
        let dir = temp_dir("grey");
        let (state, _, _) = state_with_history(&dir, N as i64);
        // The next tile along the time axis: the fixture wrote nothing there.
        let g = crate::http::with_history(&state, |p| Ok(p.geometry().clone())).unwrap();
        let key = parse_key(
            &g,
            &params(&[
                ("level_f", "0"),
                ("level_t", "0"),
                ("f_index", &F_INDEX.to_string()),
                ("t_index", &(T_INDEX + 4).to_string()),
                ("cells", &N.to_string()),
            ]),
        )
        .unwrap();
        let r = tile_read(&state, TileStore::Main, &key).unwrap();
        assert_eq!(r.grid.observed_cells, 0);
        assert!(r.grid.range_db.is_none(), "no scale from nothing");
        assert_eq!(
            r.tried, r.candidates,
            "an empty answer must have consulted every affordable level: {:?}",
            r.tried
        );
        assert!(r.candidates.len() > 1, "{:?}", r.candidates);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The cap is enforced **on the route**, not only in the slot type: with the cap's worth of
    /// readers out, the next request is a `503` that names the cap rather than a queued read that
    /// lengthens the stretch during which ingest competes for the history mutex.
    #[test]
    fn the_route_refuses_over_the_in_flight_cap_and_serves_again_once_a_slot_frees() {
        let dir = temp_dir("cap");
        let (state, _, _) = state_with_history(&dir, N as i64);
        let q = params(&[
            ("level_f", "0"),
            ("level_t", "0"),
            ("f_index", &F_INDEX.to_string()),
            ("t_index", &T_INDEX.to_string()),
            ("cells", &N.to_string()),
        ]);
        assert_eq!(
            tiles_json(&state, &q).unwrap()["cost"]["in_flight_limit"],
            json!(TILE_MAX_IN_FLIGHT)
        );
        // Two OTHER clients, each within its own share (three clients, `ceil(4/3) = 2` each), so
        // the cap is reached without any one of them exceeding what T-630 allows it.
        let held: Vec<TileSlot> = ["o1", "o1", "o2", "o2"]
            .iter()
            .map(|c| state.tile_admission.acquire(c).unwrap())
            .collect();
        assert_eq!(held.last().unwrap().in_flight(), TILE_MAX_IN_FLIGHT);
        let err = tiles_json(&state, &q).unwrap_err();
        assert_eq!(err.status, 503, "{}", err.message);
        assert!(
            err.message.contains(&TILE_MAX_IN_FLIGHT.to_string()),
            "{}",
            err.message
        );
        drop(held);
        let v = tiles_json(&state, &q).unwrap();
        assert_eq!(v["cost"]["in_flight"], json!(1), "{v}");
        assert!(v["grid"]["observed_cells"].as_u64().unwrap() > 0, "{v}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The read is chunked into whole output rows, so no single history lock hold exceeds
    /// [`TILE_MAX_SOURCE_CELLS`] — the ingest-backpressure discipline of `docs/16` §5.5 cap 3.
    #[test]
    fn a_wide_tile_is_read_in_chunks_so_one_lock_hold_stays_inside_the_budget() {
        let dir = temp_dir("chunk");
        let (state, _, _) = state_with_history(&dir, 16);
        // A tile 2^10 coarser in frequency and 2^4 longer in time: 409.6 MHz x 1024 s. `t_index`
        // is per level, so the same wall-clock window is `T_INDEX >> 4` here — which is itself the
        // de-welding: the two axes index independently.
        let (_, r) = read_at(&state, "10", "4", 1, T_INDEX >> 4);
        assert!(
            r.chunks > 1,
            "a wide tile must take more than one lock hold"
        );
        assert!(
            r.source_cells <= TILE_MAX_TOTAL_SOURCE_CELLS,
            "{} source cells",
            r.source_cells
        );
        assert!(
            r.source_cells.div_ceil(r.chunks) <= TILE_MAX_SOURCE_CELLS,
            "one lock hold read {} cells",
            r.source_cells / r.chunks
        );
        eprintln!(
            "T-438 chunking: level {}, {} source cells in {} lock hold(s)",
            r.level, r.source_cells, r.chunks
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- T-588: the band edge of a retuned window is OBSERVED, never grey ---------------------

    /// One dwell at `center_hz` / `rate_hz` — a **tuned window**, the way a retune writes one —
    /// over `[t0, t1)`. Unlike [`dwell`] this states the tuning rather than a band, because the
    /// question here is exactly what a tuning's own edge does to the coverage plane.
    fn tuned_dwell(
        center_hz: f64,
        rate_hz: f64,
        t0_ns: i64,
        t1_ns: i64,
    ) -> hk_model::attention::observation::ObservationRecord {
        let half = rate_hz / 2.0;
        let mut r = dwell(center_hz - half, center_hz + half, t0_ns, t1_ns);
        if let hk_model::attention::observation::ObservationRecord::Dwell(d) = &mut r {
            d.window.center_hz = center_hz;
            d.window.sample_rate_hz = rate_hz;
        }
        r
    }

    /// **T-588 — the user's 892.5 MHz report, as a standing guard.**
    ///
    /// A digital signal near the **band edge** vanished on zoom+retune. Two causes with completely
    /// different fixes: the emission is LO-relative and genuinely is not there at the new centre
    /// (T-586), or the backend **has** the region after the retune and the canvas greys it. The
    /// band edge is where the second is most likely, because it is where a coverage rasterisation,
    /// a tile-address rounding or an off-by-one in the observed-extent test would answer
    /// *unobserved* for a span that was in fact sampled — and grey means *the radio never looked*.
    ///
    /// The measurement was taken against a live `hk serve` over the mock SDR first (a retune from
    /// 915 MHz / 10 Msps to 919 MHz / 2 Msps and to 914 MHz / 4 Msps, then every tile of
    /// 908–922 MHz × 10 min read back); this is that measurement pinned. **The fixture ingests
    /// frames only where the radio was tuned**, era by era, so *"a cell holds a measurement"* and
    /// *"a cell was sampled"* are the same set by construction and the assertion needs no carve-out.
    ///
    /// Three assertions, and the last two are what stop it being vacuous:
    ///
    /// 1. **no cell with a `max_db` reads `unobserved`** — the invariant, counted, not sampled;
    /// 2. the cell **containing the retuned window's own edge frequency** reads `observed` — the
    ///    off-by-one, named rather than hoped for (the fold takes every cell a span *touches*, so
    ///    the edge cell is observed even though the span covers only part of it);
    /// 3. cells **beyond** that edge, in the same rows, read `unobserved` — because a test that
    ///    greys nothing would pass assertion 1 by observing everything, and grey being honest is
    ///    the other half of the same invariant.
    ///
    /// RED without the rule: changing [`hk_store::coverage`]'s fold from `ceil` to `floor` on the
    /// high edge of a span — the off-by-one this ticket went looking for — fails assertion 2 and
    /// moves assertion 1's count off zero.
    #[test]
    fn a_retuned_windows_band_edge_never_greys_a_cell_the_store_has_a_measurement_for() {
        let dir = temp_dir("t588-band-edge");
        let secs = N as i64;
        let f_cell = 6250.0;
        let tile_hz = f_cell * N as f64;

        let mut p = hk_store::Pyramid::open(&dir, PyramidConfig::default()).unwrap();
        let t_cell = p.geometry().levels[0].t_cell_ns;
        let t0 = T_INDEX * t_cell * N as i64;
        let f_lo = F_INDEX as f64 * f_cell * N as f64;

        // Era A is the wide window the user was browsing; era B is the zoom+retune, and its UPPER
        // EDGE lands a quarter of a cell past a cell boundary inside the tile — the exact place a
        // rounding error greys a band the radio was sitting on.
        let edge_hz = f_lo + f_cell * (N as f64 * 0.75) + f_cell * 0.25;
        let (rate_a, rate_b) = (tile_hz * 2.0, tile_hz / 2.0);
        let (centre_a, centre_b) = (f_lo + tile_hz / 2.0, edge_hz - rate_b / 2.0);
        let eras = [
            (centre_a, rate_a, 0, secs / 2),
            (centre_b, rate_b, secs / 2, secs),
        ];

        const NB: usize = 128;
        for (centre, rate, k0, k1) in eras {
            let bin_hz = rate / NB as f64;
            let psd = [1e-9f32; NB];
            for k in k0..k1 {
                p.ingest(&hk_store::history::FrameInput::new(
                    Timestamp::from_unix_nanos(t0 + k * t_cell),
                    t_cell,
                    centre - rate / 2.0,
                    bin_hz,
                    hk_model::PowerUnit::Dbfs,
                    &psd,
                ))
                .unwrap();
            }
        }
        let mut state = ApiState {
            history: Some(Arc::new(std::sync::Mutex::new(p))),
            ..ApiState::default()
        };
        let store = hk_store::observation::ObservationStore::open(
            hk_store::observation::ObservationLogConfig::new(dir.join("observations")),
        )
        .unwrap();
        for (centre, rate, k0, k1) in eras {
            store.append(&tuned_dwell(
                centre,
                rate,
                t0 + k0 * t_cell,
                t0 + k1 * t_cell,
            ));
        }
        store.flush();
        state.observations = Some(store);

        let v = tiles_json(&state, &tile_params(F_INDEX, T_INDEX)).unwrap();
        assert_eq!(
            v["resolution"]["short_circuit"]["applied"],
            json!(false),
            "a tile over a tuned band must not be answered from the coverage map: {v}"
        );
        let states: Vec<String> = v["coverage"]["states"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect();
        let sel = v["coverage"]["selected"]["plane"].as_u64().unwrap() as usize;
        let runs = v["coverage"]["planes"][sel]["runs"].as_array().unwrap();
        let mut plane: Vec<&str> = Vec::with_capacity(N * N);
        for pair in runs.chunks(2) {
            let s = pair[0].as_u64().unwrap() as usize;
            for _ in 0..pair[1].as_u64().unwrap() {
                plane.push(&states[s]);
            }
        }
        assert_eq!(plane.len(), N * N, "the plane is the tile's own grid: {v}");
        let max_db = v["grid"]["max_db"].as_array().unwrap();
        assert_eq!(max_db.len(), N * N, "{v}");

        // 1. The invariant, as a count over cells that actually hold a measurement.
        let measured = max_db.iter().filter(|x| !x.is_null()).count();
        let greyed: Vec<usize> = (0..N * N)
            .filter(|&i| !max_db[i].is_null() && plane[i] != "observed")
            .collect();
        assert!(
            measured >= N * N / 4,
            "the fixture must judge real measurements, not zero of zero: {measured} of {}",
            N * N
        );
        assert!(
            greyed.is_empty(),
            "{} of {measured} cells hold a measurement and are drawn grey - \
             \"we have it but didn't render it\". First at row {}, cell {} \
             ({:.4} MHz), state {:?}",
            greyed.len(),
            greyed[0] / N,
            greyed[0] % N,
            (f_lo + (greyed[0] % N) as f64 * f_cell) / 1e6,
            plane[greyed[0]]
        );

        // 2. The edge cell itself, in a row that is inside era B.
        let row_b = N * 3 / 4;
        let edge_cell = ((edge_hz - f_lo) / f_cell) as usize;
        assert_eq!(
            plane[row_b * N + edge_cell],
            "observed",
            "the cell holding the retuned window's edge ({:.4} MHz, cell {edge_cell}) is grey: {:?}",
            edge_hz / 1e6,
            &plane[row_b * N + edge_cell - 1..row_b * N + edge_cell + 2]
        );

        // 3. And grey still means something: past the edge, era B's rows are unobserved.
        assert_eq!(
            plane[row_b * N + N - 1],
            "unobserved",
            "spectrum the retuned window does not reach must stay grey, or assertion 1 \
             passes by observing everything"
        );
        assert!(
            max_db[row_b * N + N - 1].is_null(),
            "and the store must hold nothing there, or this is the bug rather than the control"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- T-595: the DC notch is EXCLUDED, never grey -----------------------------------------

    /// One dwell at `center_hz` / `rate_hz` with the producer's **DC notch declared**, the way a
    /// real dwell record is written (`hk-core`'s `scheduler::observe`, `DcRule`'s ±15 kHz).
    fn notched_dwell(
        center_hz: f64,
        rate_hz: f64,
        dc_half_hz: f64,
        t0_ns: i64,
        t1_ns: i64,
    ) -> hk_model::attention::observation::ObservationRecord {
        let mut r = tuned_dwell(center_hz, rate_hz, t0_ns, t1_ns);
        if let hk_model::attention::observation::ObservationRecord::Dwell(d) = &mut r {
            d.window.dc_excluded = Some(FreqRange::new(
                center_hz - dc_half_hz,
                center_hz + dc_half_hz,
            ));
        }
        r
    }

    /// **T-595 — the 25 kHz stripe down the middle of every band the radio ever sat on.**
    ///
    /// Measured by T-588 over a sweep of 1 966 080 cells: 1 212 cells held a measurement and read
    /// `unobserved`, and **all 1 212 were the DC notch at the tuned centre**. The observation log
    /// punched a hole in its own coverage (`ObservedWindow.dc_excluded`) while the history pyramid
    /// kept rows right across it, so past the IQ ring's horizon — where the ring journal's
    /// full-band segments no longer cover the hole — the canvas painted *"we never looked"* over
    /// spectrum we hold. Both halves of the invariant at once: data that exists was not shown, and
    /// grey stopped meaning genuinely unobserved.
    ///
    /// **This fixture has no IQ ring**, which is exactly what *past the ring horizon* means to the
    /// coverage map: the observation log is the only evidence left. That is why nobody saw this
    /// until history aged out — a test with a ring passes while the defect is intact.
    ///
    /// The notch gets its own mark, `"excluded"`: sampled, and deliberately left out of analysis.
    /// Not `"observed"` (the detector really did ignore it, and an absence of detections there is
    /// not a finding), and above all not `"unobserved"`.
    ///
    /// Four assertions, counted, with floors so none can pass on zero of zero:
    ///
    /// 1. **no cell holding a `max_db` reads `unobserved` or `unknown`** — the invariant;
    /// 2. the notch cells read `"excluded"`, and there are some;
    /// 3. the analysed band around them still reads `"observed"` — an exclusion that spread would
    ///    be the same defect pointing the other way;
    /// 4. spectrum the dwell never reached is still grey, and the store holds nothing there.
    ///
    /// RED without the fix: drop the `Analysis::Excluded` span in
    /// [`hk_store::coverage::spans_from_records`] and assertion 1 counts the notch cells as grey.
    #[test]
    fn the_dc_notch_past_the_ring_horizon_is_excluded_never_a_cell_greyed_over_a_measurement() {
        let dir = temp_dir("t595-dc-notch");
        let secs = N as i64;
        let f_cell = 6250.0;
        let tile_hz = f_cell * N as f64;
        let dc_half = 15e3;

        let mut p = hk_store::Pyramid::open(&dir, PyramidConfig::default()).unwrap();
        let t_cell = p.geometry().levels[0].t_cell_ns;
        let t0 = T_INDEX * t_cell * N as i64;
        let f_lo = F_INDEX as f64 * f_cell * N as f64;

        // The dwell covers the lower three quarters of the tile, so the top quarter is the control:
        // never tuned, nothing ingested, honestly grey.
        let rate = tile_hz * 0.75;
        let centre = f_lo + rate / 2.0;

        // Frames span the WHOLE tuned band, notch included — that is what the FFT produces and what
        // the pyramid keeps. The notch is an analysis exclusion, not a gap in the spectrum rows.
        const NB: usize = 256;
        let bin_hz = rate / NB as f64;
        let psd = [1e-9f32; NB];
        for k in 0..secs {
            p.ingest(&hk_store::history::FrameInput::new(
                Timestamp::from_unix_nanos(t0 + k * t_cell),
                t_cell,
                centre - rate / 2.0,
                bin_hz,
                hk_model::PowerUnit::Dbfs,
                &psd,
            ))
            .unwrap();
        }
        let mut state = ApiState {
            history: Some(Arc::new(std::sync::Mutex::new(p))),
            // No IQ ring: the segment journal that used to paper over the notch is gone, which is
            // the state of every window older than the ring's retention.
            iq_buffer: None,
            ..ApiState::default()
        };
        let store = hk_store::observation::ObservationStore::open(
            hk_store::observation::ObservationLogConfig::new(dir.join("observations")),
        )
        .unwrap();
        store.append(&notched_dwell(
            centre,
            rate,
            dc_half,
            t0,
            t0 + secs * t_cell,
        ));
        store.flush();
        state.observations = Some(store);

        let v = tiles_json(&state, &tile_params(F_INDEX, T_INDEX)).unwrap();
        let states: Vec<String> = v["coverage"]["states"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect();
        let sel = v["coverage"]["selected"]["plane"].as_u64().unwrap() as usize;
        let runs = v["coverage"]["planes"][sel]["runs"].as_array().unwrap();
        let mut plane: Vec<&str> = Vec::with_capacity(N * N);
        for pair in runs.chunks(2) {
            let s = pair[0].as_u64().unwrap() as usize;
            for _ in 0..pair[1].as_u64().unwrap() {
                plane.push(&states[s]);
            }
        }
        assert_eq!(plane.len(), N * N, "the plane is the tile's own grid: {v}");
        let max_db = v["grid"]["max_db"].as_array().unwrap();
        assert_eq!(max_db.len(), N * N, "{v}");

        // 1. The invariant, counted: nothing we hold a measurement for is drawn as never-looked-at.
        let measured = max_db.iter().filter(|x| !x.is_null()).count();
        assert!(
            measured >= N * N / 2,
            "the fixture must judge real measurements, not zero of zero: {measured} of {}",
            N * N
        );
        let greyed: Vec<usize> = (0..N * N)
            .filter(|&i| {
                !max_db[i].is_null() && (plane[i] == "unobserved" || plane[i] == "unknown")
            })
            .collect();
        assert!(
            greyed.is_empty(),
            "{} of {measured} cells hold a measurement and are drawn grey - \
             \"we have it but didn't render it\". First at row {}, cell {} ({:.4} MHz), state {:?}",
            greyed.len(),
            greyed[0] / N,
            greyed[0] % N,
            (f_lo + (greyed[0] % N) as f64 * f_cell) / 1e6,
            plane[greyed[0]]
        );

        // 2. And the notch is not silently reclassified as ordinary coverage: the cells wholly
        // inside it carry the exclusion, in every row the dwell covers.
        let inside: Vec<usize> = (0..N)
            .filter(|&f| {
                let lo = f_lo + f as f64 * f_cell;
                lo >= centre - dc_half && lo + f_cell <= centre + dc_half
            })
            .collect();
        assert!(
            inside.len() >= 2,
            "the fixture must judge notch cells: {inside:?}"
        );
        let row = N / 2;
        for &f in &inside {
            assert_eq!(
                plane[row * N + f],
                "excluded",
                "the DC notch cell at {:.4} MHz must read \"excluded\": {:?}",
                (f_lo + f as f64 * f_cell) / 1e6,
                &plane[row * N + f - 1..row * N + f + 2]
            );
            assert!(
                !max_db[row * N + f].is_null(),
                "and the store holds a measurement there - otherwise this is not the cell the \
                 ticket is about"
            );
        }
        let excluded = (0..N * N).filter(|&i| plane[i] == "excluded").count();
        assert_eq!(
            excluded,
            inside.len() * N,
            "the exclusion is exactly the notch, in every row: {excluded}"
        );

        // 3. The analysed band is still analysed.
        assert_eq!(
            plane[row * N],
            "observed",
            "{:?}",
            &plane[row * N..row * N + 4]
        );

        // 4. Grey still means something: the quarter the dwell never reached.
        assert_eq!(
            plane[row * N + N - 1],
            "unobserved",
            "spectrum the dwell does not reach must stay grey, or assertion 1 passes by \
             observing everything"
        );
        assert!(
            max_db[row * N + N - 1].is_null(),
            "and the store must hold nothing there, or this is the bug rather than the control"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- T-596: the live edge with the IQ ring refused ----------------------------------------

    /// **T-596 — a full disk must not make the canvas lie about where the radio looked.**
    ///
    /// Measured by T-588 with `iq_buffer: None`: **18 rows (18 s) of `max_db` carried
    /// `state: unobserved`** right after a retune. The observation log gains a record only when a
    /// dwell **seals** — on the next retune, or after `INTERACTIVE_RECORD_MAX_NS` (60 s) of a
    /// steady tune — so between the retune and the seal the log says nothing about the band the
    /// radio is sitting on. With an IQ ring that gap is covered by the ring journal, whose
    /// segments open on every provenance change. **The ring was refused because the disk was
    /// full** ("needs 134217728 bytes above the 8589934592-byte free-space floor and 4789297152
    /// bytes are free"), and a portable device *will* fill its disk: this is the field failure
    /// mode, and the honest way to survive it is to say what the radio is on, not to grey it.
    ///
    /// Same invariant as T-595 — data exists and is drawn grey — but the **live edge** rather than
    /// the history horizon, and it composes with T-595 rather than competing: the dwell in flight
    /// is *the same claim* as the sealed one, so it gets no mark of its own (assertion 1), and it
    /// declares the same DC notch, so the notch still reads `"excluded"` at the live edge and does
    /// not flip when the seal catches up (assertion 2).
    ///
    /// The fixture is the retune T-588 drove: era A is sealed in the log, era B is the tune the
    /// radio is on *now* and exists only as the open dwell. **No IQ ring at all** — the refusal.
    ///
    /// Assertions, counted, with floors so none can pass on zero of zero:
    ///
    /// 1. **no cell holding a `max_db` reads `unobserved` or `unknown`**, over ≥ `N*N/4` measured
    ///    cells, of which ≥ `N*N/8` are in era B's rows — the count T-588 measured as 18;
    /// 2. era B's DC notch reads `"excluded"` (T-595's mark, at the live edge);
    /// 3. era B's rows are `"observed"` where the open dwell reaches, so the evidence is really
    ///    the open dwell and not era A's sealed record leaking forward;
    /// 4. grey still means something: spectrum era B never reached stays `"unobserved"` and the
    ///    store holds nothing there;
    /// 5. the answer **says** which evidence carried it: `sources` reports the IQ ring
    ///    unavailable and the `open-dwell` source non-empty.
    ///
    /// RED without the fix: drop the `open_dwell_spans` call from `Evidence::collect` and
    /// assertion 1 counts every era-B cell as grey (measured: 1024 of 2048).
    #[test]
    fn the_live_edge_is_observed_when_the_iq_ring_is_refused_and_the_dwell_has_not_sealed() {
        let dir = temp_dir("t596-open-dwell");
        let secs = N as i64;
        let f_cell = 6250.0;
        let tile_hz = f_cell * N as f64;
        let dc_half = 15e3;

        let mut p = hk_store::Pyramid::open(&dir, PyramidConfig::default()).unwrap();
        let t_cell = p.geometry().levels[0].t_cell_ns;
        let t0 = T_INDEX * t_cell * N as i64;
        let f_lo = F_INDEX as f64 * f_cell * N as f64;

        // Era A: the lower half, sealed. Era B: the retune the radio is on now — the middle half,
        // so it overlaps era A (proving the rows are era B's own coverage) and stops a quarter
        // short of the top, which is the never-tuned control.
        let (rate_a, rate_b) = (tile_hz / 2.0, tile_hz / 2.0);
        let (centre_a, centre_b) = (f_lo + tile_hz / 4.0, f_lo + tile_hz / 2.0);
        let eras = [
            (centre_a, rate_a, 0, secs / 2),
            (centre_b, rate_b, secs / 2, secs),
        ];

        const NB: usize = 256;
        for (centre, rate, k0, k1) in eras {
            let bin_hz = rate / NB as f64;
            let psd = [1e-9f32; NB];
            for k in k0..k1 {
                p.ingest(&hk_store::history::FrameInput::new(
                    Timestamp::from_unix_nanos(t0 + k * t_cell),
                    t_cell,
                    centre - rate / 2.0,
                    bin_hz,
                    hk_model::PowerUnit::Dbfs,
                    &psd,
                ))
                .unwrap();
            }
        }
        let mut state = ApiState {
            history: Some(Arc::new(std::sync::Mutex::new(p))),
            // The refusal. A full disk leaves the server with no ring journal at all, and the
            // observation log is then the only evidence of where the radio looked.
            iq_buffer: None,
            ..ApiState::default()
        };
        let store = hk_store::observation::ObservationStore::open(
            hk_store::observation::ObservationLogConfig::new(dir.join("observations")),
        )
        .unwrap();
        // Era A sealed; era B is the dwell in flight, exactly as the observer publishes it.
        store.append(&notched_dwell(
            centre_a,
            rate_a,
            dc_half,
            t0,
            t0 + (secs / 2) * t_cell,
        ));
        store.flush();
        let open = notched_dwell(
            centre_b,
            rate_b,
            dc_half,
            t0 + (secs / 2) * t_cell,
            t0 + secs * t_cell,
        );
        let hk_model::attention::observation::ObservationRecord::Dwell(open) = open else {
            unreachable!("notched_dwell builds a dwell")
        };
        store.note_open_dwell(open);
        assert!(
            store
                .query(&hk_store::observation::RecordQuery {
                    freq: FreqRange::new(f_lo, f_lo + tile_hz),
                    span: hk_model::TimeRange::new(
                        Timestamp::from_unix_nanos(t0),
                        Timestamp::from_unix_nanos(t0 + secs * t_cell),
                    ),
                    tier: None,
                    cursor: 0,
                    limit: 64,
                })
                .records
                .len()
                == 1,
            "the open dwell must NOT be a queryable record: a provisional record must not reach \
             the occupancy, POI or report paths that count sealed visits"
        );
        state.observations = Some(store);

        let v = tiles_json(&state, &tile_params(F_INDEX, T_INDEX)).unwrap();
        let states: Vec<String> = v["coverage"]["states"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect();
        let sel = v["coverage"]["selected"]["plane"].as_u64().unwrap() as usize;
        let runs = v["coverage"]["planes"][sel]["runs"].as_array().unwrap();
        let mut plane: Vec<&str> = Vec::with_capacity(N * N);
        for pair in runs.chunks(2) {
            let s = pair[0].as_u64().unwrap() as usize;
            for _ in 0..pair[1].as_u64().unwrap() {
                plane.push(&states[s]);
            }
        }
        assert_eq!(plane.len(), N * N, "the plane is the tile's own grid: {v}");
        let max_db = v["grid"]["max_db"].as_array().unwrap();
        assert_eq!(max_db.len(), N * N, "{v}");

        // 1. The invariant, counted over both eras and over era B on its own — the live edge is
        //    the half that had no evidence at all, so a floor on the whole tile is not enough.
        let row_b0 = N / 2;
        let measured = max_db.iter().filter(|x| !x.is_null()).count();
        let measured_b = (row_b0 * N..N * N)
            .filter(|&i| !max_db[i].is_null())
            .count();
        assert!(
            measured >= N * N / 4 && measured_b >= N * N / 8,
            "the fixture must judge real measurements, not zero of zero: {measured} of {}, \
             {measured_b} of them at the live edge",
            N * N
        );
        let greyed: Vec<usize> = (0..N * N)
            .filter(|&i| {
                !max_db[i].is_null() && (plane[i] == "unobserved" || plane[i] == "unknown")
            })
            .collect();
        assert!(
            greyed.is_empty(),
            "{} of {measured} cells hold a measurement and are drawn grey - a full disk made the \
             canvas lie about where the radio looked. First at row {}, cell {} ({:.4} MHz), \
             state {:?}",
            greyed.len(),
            greyed[0] / N,
            greyed[0] % N,
            (f_lo + (greyed[0] % N) as f64 * f_cell) / 1e6,
            plane[greyed[0]]
        );

        // 2. T-595's mark survives at the live edge: the open dwell declares the same notch.
        let inside: Vec<usize> = (0..N)
            .filter(|&f| {
                let lo = f_lo + f as f64 * f_cell;
                lo >= centre_b - dc_half && lo + f_cell <= centre_b + dc_half
            })
            .collect();
        assert!(
            inside.len() >= 2,
            "the fixture must judge notch cells: {inside:?}"
        );
        let row = row_b0 + N / 4;
        for &f in &inside {
            assert_eq!(
                plane[row * N + f],
                "excluded",
                "the open dwell's DC notch at {:.4} MHz must read \"excluded\", the same mark it \
                 will carry once it seals: {:?}",
                (f_lo + f as f64 * f_cell) / 1e6,
                &plane[row * N + f - 1..row * N + f + 2]
            );
        }

        // 3. Era B's own band is observed at a frequency era A never covered — so the evidence is
        //    the open dwell, not the sealed record leaking forward in time.
        let past_a = ((tile_hz * 0.625) / f_cell) as usize;
        assert_eq!(
            plane[row * N + past_a],
            "observed",
            "{:.4} MHz is inside era B and outside era A: {:?}",
            (f_lo + past_a as f64 * f_cell) / 1e6,
            &plane[row * N + past_a - 1..row * N + past_a + 2]
        );

        // 4. Grey still means something: the top quarter, which no era ever reached.
        assert_eq!(
            plane[row * N + N - 1],
            "unobserved",
            "spectrum the radio never tuned must stay grey, or assertion 1 passes by observing \
             everything"
        );
        assert!(
            max_db[row * N + N - 1].is_null(),
            "and the store must hold nothing there, or this is the bug rather than the control"
        );

        // 5. The answer says which evidence carried it.
        let sources = v["coverage"]["sources"].as_array().unwrap();
        let src = |kind: &str| -> Value {
            sources
                .iter()
                .find(|s| s["kind"] == json!(kind))
                .unwrap_or_else(|| panic!("source {kind} missing: {sources:?}"))
                .clone()
        };
        assert_eq!(
            src("iq-ring")["available"],
            json!(false),
            "the ring was refused, and the answer must say so"
        );
        assert!(
            src("open-dwell")["spans"].as_u64().unwrap() > 0,
            "the live edge was carried by the dwell in flight, and the answer must say so: {:?}",
            src("open-dwell")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- T-461: the coverage short-circuit ---------------------------------------------------

    /// One dwell over `lo..hi` for `[t0, t1)`, by a named front end — the record that makes a band
    /// *observed* and, just as importantly, fixes the **record horizon**: a tile wholly before it
    /// is `"unknown"`, not `"unobserved"`, and must not short-circuit.
    fn dwell(
        lo: f64,
        hi: f64,
        t0_ns: i64,
        t1_ns: i64,
    ) -> hk_model::attention::observation::ObservationRecord {
        use hk_model::attention::baseline::SiteKey;
        use hk_model::attention::observation::{
            DwellRecord, ObservationRecord, ObservedWindow, Reason, Tier,
        };
        let w = TimeRange::new(
            Timestamp::from_unix_nanos(t0_ns),
            Timestamp::from_unix_nanos(t1_ns),
        );
        ObservationRecord::Dwell(DwellRecord {
            schema: hk_model::attention::ATTENTION_SCHEMA_VERSION,
            survey_id: None,
            seq: 1,
            plan_version: 1,
            site: SiteKey::Unassigned,
            device_id: Some("mock:0".into()),
            reason: Reason::RegionDwell { hop: 0 },
            tier: Tier::ScheduledPlan,
            window: ObservedWindow {
                center_hz: (lo + hi) / 2.0,
                sample_rate_hz: hi - lo,
                usable: FreqRange::new(lo, hi),
                dc_excluded: None,
                rbw_hz: 1e3,
            },
            rf_path: 0,
            planned: w,
            observed: w,
            preempted: false,
            dropped_samples: 0,
            overload: false,
            provenance_ref: None,
        })
    }

    /// [`state_with_history`] plus an observation log holding one dwell over the fixture's own band
    /// and window, offset in time by `record_offset_s`.
    ///
    /// The offset is the whole point of the fixture: at `0` the record covers the tile, so the
    /// fixture's band is `observed` and every other band is `unobserved`. Pushed an hour into the
    /// future, the record horizon moves past the tile and *every* cell becomes `"unknown"` — which
    /// is the fail-closed case, and it is a case this fixture can actually produce rather than one
    /// argued for.
    fn state_with_records(
        dir: &std::path::Path,
        secs: i64,
        record_offset_s: i64,
    ) -> (ApiState, i64, f64) {
        let (mut state, t0, f_lo) = state_with_history(dir, secs);
        let f_hi = f_lo + 6250.0 * N as f64;
        let store = hk_store::observation::ObservationStore::open(
            hk_store::observation::ObservationLogConfig::new(dir.join("observations")),
        )
        .unwrap();
        let off = record_offset_s * 1_000_000_000;
        store.append(&dwell(
            f_lo,
            f_hi,
            t0 + off,
            t0 + off + secs * 1_000_000_000,
        ));
        store.flush();
        state.observations = Some(store);
        (state, t0, f_lo)
    }

    fn tile_params(f_index: i64, t_index: i64) -> Vec<(String, String)> {
        params(&[
            ("level_f", "0"),
            ("level_t", "0"),
            ("f_index", &f_index.to_string()),
            ("t_index", &t_index.to_string()),
            ("cells", &N.to_string()),
        ])
    }

    // -----------------------------------------------------------------------------------------
    // T-579: the per-request tile geometry, memoised.
    // -----------------------------------------------------------------------------------------

    /// One screen's worth of tiles: `F_INDEX-1 ..= F_INDEX+2` × `{T_INDEX-1, T_INDEX}`.
    fn viewport() -> Vec<(i64, i64)> {
        let mut v = Vec::new();
        for t in [T_INDEX - 1, T_INDEX] {
            for f in F_INDEX - 1..=F_INDEX + 2 {
                v.push((f, t));
            }
        }
        v
    }

    fn poll(state: &ApiState) -> Vec<Value> {
        viewport()
            .into_iter()
            .map(|(f, t)| tiles_json(state, &tile_params(f, t)).unwrap())
            .collect()
    }

    /// **Counts, not wall clock** (T-579): serving an N-tile viewport probes the lattice ONCE, not
    /// N times; re-polling it rasterises coverage ZERO more times; and a tune-history change that
    /// reaches one tile re-rasterises exactly THAT tile, exactly once — and its answer changes.
    /// The last half is what keeps the cache honest: a memo that never invalidated would pass the
    /// "cheap" half while serving grey where data now exists.
    #[test]
    fn a_viewport_probes_the_lattice_once_and_rasterises_coverage_once_per_history_change() {
        let dir = temp_dir("t579-memo");
        let (state, t0, f_lo) = state_with_records(&dir, (N as i64) + 36, 0);
        let n = viewport().len() as u64;

        let first = poll(&state);
        assert_eq!(
            state.ceiling_memo.computations(),
            1,
            "an {n}-tile viewport probed the whole lattice more than once"
        );
        assert_eq!(
            state.coverage_raster.rasterisations(),
            n,
            "each distinct tile is rasterised once on first sight"
        );

        for round in 0..3 {
            let again = poll(&state);
            assert_eq!(state.ceiling_memo.computations(), 1, "round {round}");
            assert_eq!(
                state.coverage_raster.rasterisations(),
                n,
                "round {round}: an unchanged tune history was rasterised again"
            );
            for (a, b) in first.iter().zip(&again) {
                assert_eq!(
                    a["coverage"], b["coverage"],
                    "a memo hit changed the answer"
                );
                assert_eq!(a["axes"], b["axes"], "a memo hit changed the ceiling");
            }
        }
        assert_eq!(state.coverage_raster.hits(), 3 * n);

        // The tune history changes INSIDE one tile, (F_INDEX, T_INDEX): a second front end dwells
        // on the middle of its band for ten seconds of its window.
        let changed = viewport()
            .iter()
            .position(|&k| k == (F_INDEX, T_INDEX))
            .unwrap();
        let mid = f_lo + 6250.0 * N as f64 / 2.0;
        let mut rec = dwell(
            mid - 20e3,
            mid + 20e3,
            t0 + 10_000_000_000,
            t0 + 20_000_000_000,
        );
        if let hk_model::attention::observation::ObservationRecord::Dwell(d) = &mut rec {
            d.device_id = Some("mock:1".into());
        }
        let log = state.observations.as_ref().unwrap();
        log.append(&rec);
        log.flush();

        let after = poll(&state);
        assert_eq!(
            state.coverage_raster.rasterisations(),
            n + 1,
            "the tune-history change must re-rasterise the one tile it reaches, exactly once"
        );
        let devices = |v: &Value| v["coverage"]["devices"].as_array().unwrap().len();
        assert_eq!(
            devices(&after[changed]),
            devices(&first[changed]) + 1,
            "the changed tile must SHOW the new front end's coverage, not the cached answer"
        );
        for (i, (a, b)) in first.iter().zip(&after).enumerate() {
            if i != changed {
                assert_eq!(a["coverage"]["planes"], b["coverage"]["planes"], "tile {i}");
            }
        }
        poll(&state);
        assert_eq!(
            state.coverage_raster.rasterisations(),
            n + 1,
            "once re-rasterised, the new history is memoised too"
        );
        assert_eq!(state.ceiling_memo.computations(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------------------------
    // T-572: the hot-tile LRU.
    // -----------------------------------------------------------------------------------------

    /// Filesystem reads the pyramid has done, and a way to zero it — the COUNT this ticket is
    /// asserted with, rather than a wall clock.
    fn source_reads(state: &ApiState) -> u64 {
        state
            .history
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .source_tiles_read()
    }
    fn reset_source_reads(state: &ApiState) {
        state
            .history
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .reset_source_tiles_read();
    }

    /// **A repeated viewport poll of a SEALED tile reads the filesystem zero times** (T-572).
    ///
    /// And the answer is the same answer: a cache that served a different tile cheaply would be
    /// worse than no cache. Counts, not wall clock — `source_tiles_read` is the pyramid's own
    /// count of source tiles pulled off disk.
    #[test]
    fn a_repeated_read_of_a_sealed_tile_touches_the_filesystem_zero_times() {
        let dir = temp_dir("cache-sealed");
        // Past the tile's own 64 s extent, so the watermark has sealed it.
        let (mut state, _, _) = state_with_history(&dir, (N as i64) + 36);
        state.tile_cache = Some(Arc::new(HotTileCache::default()));

        let first = tiles_json(&state, &tile_params(F_INDEX, T_INDEX)).unwrap();
        assert_eq!(
            first["sealed"],
            json!(true),
            "the fixture must seal this tile"
        );
        assert!(
            first["cost"]["served_from"].is_null(),
            "the first read is a real read"
        );
        reset_source_reads(&state);

        for poll in 0..5 {
            let again = tiles_json(&state, &tile_params(F_INDEX, T_INDEX)).unwrap();
            assert_eq!(
                source_reads(&state),
                0,
                "poll {poll} went to the filesystem for a sealed tile"
            );
            assert_eq!(again["cost"]["served_from"], json!("hot-tile-cache"));
            // The SAME answer, not a cheaper one.
            for f in [
                "key", "extent", "axes", "grid", "coverage", "sealed", "shadow",
            ] {
                assert_eq!(again[f], first[f], "{f} changed across a cache hit");
            }
            // …except this read's own diagnostics, which are this read's.
            assert!(again["cost"]["in_flight"].is_number());
        }

        let stats = state.tile_cache.as_ref().unwrap().stats_json();
        assert_eq!(stats["hits"], json!(5), "{stats}");
        assert_eq!(stats["entries"], json!(1), "{stats}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A LIVE tile is re-read every time, and that is the correctness half of the ticket.**
    ///
    /// The growing edge changes on every arriving row: serving a stale copy would break *"the live
    /// view renders like a classic SDR waterfall, rows append in real time"* exactly as badly as
    /// serving nothing. The distinction is structural — a live tile is never looked up and never
    /// inserted — so this asserts the filesystem is reached on EVERY poll, and that the cache
    /// never grew an entry for it.
    #[test]
    fn a_live_tile_is_never_cached_and_is_re_read_on_every_poll() {
        let dir = temp_dir("cache-live");
        let (mut state, _, _) = state_with_history(&dir, (N as i64) + 36);
        state.tile_cache = Some(Arc::new(HotTileCache::default()));

        // The tile the watermark is INSIDE: its extent reaches past the newest frame.
        let live = tiles_json(&state, &tile_params(F_INDEX, T_INDEX + 1)).unwrap();
        assert_eq!(
            live["sealed"],
            json!(false),
            "the fixture must leave this tile growing"
        );

        for poll in 0..3 {
            reset_source_reads(&state);
            let v = tiles_json(&state, &tile_params(F_INDEX, T_INDEX + 1)).unwrap();
            assert!(
                v["cost"]["served_from"].is_null(),
                "poll {poll} was served from cache at the live edge"
            );
            assert!(
                source_reads(&state) > 0,
                "poll {poll} did not re-read the growing tile"
            );
        }
        let stats = state.tile_cache.as_ref().unwrap().stats_json();
        assert_eq!(
            stats["entries"],
            json!(0),
            "a live tile was cached: {stats}"
        );
        assert_eq!(stats["hits"], json!(0), "{stats}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The bound is in BYTES and in ENTRIES, and it holds under sustained scrolling** (T-453).
    ///
    /// Residency must not grow with node count, so neither bound is a function of the pyramid: the
    /// cache is asserted against a cap it is driven far past, with eviction counted rather than
    /// inferred. Driven directly, because driving it through the route would need hundreds of real
    /// tiles to say the same thing.
    #[test]
    fn the_cache_stays_inside_its_stated_bound_however_long_the_scroll_is() {
        let c = HotTileCache::default();
        let epoch = (7, 3);
        // ~4 kB an entry, four times the entry cap: the entry bound binds first and evicts.
        let body = json!({ "grid": vec![-80.0f64; 400] });
        for i in 0..TILE_CACHE_MAX_ENTRIES * 4 {
            c.put(format!("tile-{i}"), &body, epoch);
        }
        let s = c.stats_json();
        assert_eq!(s["entries"], json!(TILE_CACHE_MAX_ENTRIES), "{s}");
        assert!(
            s["bytes"].as_u64().unwrap() <= TILE_CACHE_MAX_BYTES as u64,
            "{s}"
        );
        assert_eq!(
            s["evictions"],
            json!(TILE_CACHE_MAX_ENTRIES as u64 * 3),
            "eviction is counted, not inferred: {s}"
        );
        // Least-recently-USED, not least-recently-inserted: touching an old key keeps it.
        let c = HotTileCache::default();
        for i in 0..TILE_CACHE_MAX_ENTRIES {
            c.put(format!("k{i}"), &body, epoch);
        }
        assert!(c.get("k0", epoch).is_some());
        c.put("fresh".into(), &body, epoch);
        assert!(
            c.get("k0", epoch).is_some(),
            "the touched entry was evicted"
        );
        assert!(c.get("k1", epoch).is_none(), "the coldest entry survived");

        // And a body larger than the whole cache is never held: the bound is unconditional.
        let c = HotTileCache::default();
        let huge = json!({ "grid": vec![-80.0f64; TILE_CACHE_MAX_BYTES / 4] });
        c.put("huge".into(), &huge, epoch);
        let s = c.stats_json();
        assert_eq!(s["entries"], json!(0), "{s}");
        assert_eq!(s["bytes"], json!(0), "{s}");
    }

    /// **The coverage plane beside a sealed grid can still move, and the epoch catches it.**
    ///
    /// A sealed tile's grid can never change again, but its `coverage` is derived from the
    /// observation log — appended to as capture proceeds, pruned by retention. Any movement in
    /// either counter drops every entry, so a cached answer can never outlive the coverage it
    /// states. Invalidation is counted, so it is observable rather than argued.
    #[test]
    fn the_cache_is_dropped_whenever_the_observation_log_moves() {
        let c = HotTileCache::default();
        let body = json!({ "coverage": "observed" });
        c.put("t".into(), &body, (1, 0));
        assert!(c.get("t", (1, 0)).is_some());
        // A record appended.
        assert!(
            c.get("t", (2, 0)).is_none(),
            "a new record left a stale coverage answer"
        );
        c.put("t".into(), &body, (2, 0));
        // A segment pruned.
        assert!(
            c.get("t", (2, 1)).is_none(),
            "retention left a stale coverage answer"
        );
        let s = c.stats_json();
        assert_eq!(s["invalidations"], json!(2), "{s}");
        assert_eq!(s["entries"], json!(0), "{s}");
    }

    // -----------------------------------------------------------------------------------------
    // T-581: the in-flight cap is a PRODUCER cap; a cached sealed tile needs no slot.
    // -----------------------------------------------------------------------------------------

    /// One screen around the fixture's sealed tile: 4 frequency x 3 time addresses, every one of
    /// them sealed (its whole extent is behind the watermark).
    fn sealed_screen() -> Vec<(i64, i64)> {
        let mut v = Vec::new();
        for t in T_INDEX - 2..=T_INDEX {
            for f in F_INDEX - 2..=F_INDEX + 1 {
                v.push((f, t));
            }
        }
        v
    }

    fn viewer_params(f_index: i64, t_index: i64) -> Vec<(String, String)> {
        let mut q = tile_params(f_index, t_index);
        q.push(("client".into(), "viewer".into()));
        q
    }

    /// Two slow coarse producers of two OTHER clients holding every slot, each inside its own
    /// share — the state T-450 measured the fill collapsing behind.
    fn slow_producers_hold_the_cap(state: &ApiState) -> Vec<TileSlot> {
        let held: Vec<TileSlot> = ["slow-a", "slow-a", "slow-b", "slow-b"]
            .iter()
            .map(|c| state.tile_admission.acquire(c).unwrap())
            .collect();
        assert_eq!(held.last().unwrap().in_flight(), TILE_MAX_IN_FLIGHT);
        held
    }

    /// **A steady-state fill of a drawn screen issues ZERO 503s behind slow producers** (T-581).
    ///
    /// Counts, not wall clock. A screen of 12 sealed tiles is drawn once, then every producer slot
    /// is taken by two other clients' slow coarse reads, then the screen is re-polled three times
    /// by `2 x TILE_MAX_IN_FLIGHT` concurrent readers. Before T-581 every one of those 36 reads was
    /// a `503` — each a halving of the client's operating limit — although not one of them needed
    /// the store. Now: 36 admitted, 0 refused, 0 source tiles read, and the refused-to-resident
    /// ratio for the screen is 0 (bound: 0).
    #[test]
    fn a_steady_state_fill_behind_slow_producers_issues_no_503s() {
        let dir = temp_dir("t581-fill");
        let (mut state, _, _) = state_with_history(&dir, (N as i64) + 36);
        state.tile_cache = Some(Arc::new(HotTileCache::default()));
        let screen = sealed_screen();
        for &(f, t) in &screen {
            let v = tiles_json(&state, &viewer_params(f, t)).unwrap();
            assert_eq!(v["sealed"], json!(true), "({f}, {t}) must be sealed: {v}");
        }
        let held = slow_producers_hold_the_cap(&state);
        reset_source_reads(&state);

        const POLLS: usize = 3;
        let readers = 2 * TILE_MAX_IN_FLIGHT;
        let admitted = AtomicUsize::new(0);
        let refused = AtomicUsize::new(0);
        let other = AtomicUsize::new(0);
        let peak_unslotted = AtomicUsize::new(0);
        let asks: Vec<(i64, i64)> = (0..POLLS).flat_map(|_| screen.clone()).collect();
        let next = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..readers {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::AcqRel);
                        let Some(&(f, t)) = asks.get(i) else { break };
                        match tiles_json(&state, &viewer_params(f, t)) {
                            Ok(v) => {
                                admitted.fetch_add(1, Ordering::AcqRel);
                                assert_eq!(v["cost"]["served_from"], json!("hot-tile-cache"));
                                assert_eq!(v["cost"]["in_flight_held"], json!(0), "{v}");
                                peak_unslotted.fetch_max(
                                    v["cost"]["in_flight"].as_u64().unwrap() as usize,
                                    Ordering::AcqRel,
                                );
                            }
                            Err(e) if e.status == 503 => {
                                refused.fetch_add(1, Ordering::AcqRel);
                            }
                            Err(_) => {
                                other.fetch_add(1, Ordering::AcqRel);
                            }
                        }
                    }
                });
            }
        });
        let (admitted, refused) = (admitted.into_inner(), refused.into_inner());
        let exercised = screen.len() * POLLS;
        // Not 0 of 0: this run asked 36 times, from 8 readers, with 4 slots out.
        assert_eq!(exercised, 36);
        assert_eq!(other.into_inner(), 0);
        assert_eq!(
            admitted + refused,
            exercised,
            "every ask must be answered one way or the other"
        );
        assert_eq!(
            refused, 0,
            "{refused} of {exercised} steady-state reads of a drawn screen were refused 503 \
             behind {TILE_MAX_IN_FLIGHT} slow producers; each is a halving of the client's limit"
        );
        assert_eq!(admitted, exercised);
        // The ratio the ticket bounds, over this one screen: refusals (what the client cancels
        // and re-asks) per resident tile.
        assert_eq!(refused as f64 / screen.len() as f64, 0.0);
        assert_eq!(
            state.tile_admission.hot_answers(),
            exercised,
            "each admitted read was a slot-less hot answer"
        );
        // The producer cap itself never moved: the slow producers still hold all of it.
        assert_eq!(held[0].in_flight(), TILE_MAX_IN_FLIGHT);
        assert_eq!(
            peak_unslotted.into_inner(),
            TILE_MAX_IN_FLIGHT,
            "a hot answer must not take a slot"
        );
        assert_eq!(
            source_reads(&state),
            0,
            "a hot answer must not read the store"
        );
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The cap still binds every read that would PRODUCE** (T-581's other half): a LIVE tile is
    /// never cached, so with every slot out it is refused `503`, naming the cap, exactly as
    /// before — and so is a sealed tile nobody has read yet. Only a cached sealed answer escapes.
    #[test]
    fn a_live_or_uncached_tile_still_meets_the_producer_cap() {
        let dir = temp_dir("t581-cap");
        let (mut state, _, _) = state_with_history(&dir, (N as i64) + 36);
        state.tile_cache = Some(Arc::new(HotTileCache::default()));
        // The live tile, read once so a cache COULD have held it if the policy were wrong.
        let live = tiles_json(&state, &viewer_params(F_INDEX, T_INDEX + 1)).unwrap();
        assert_eq!(live["sealed"], json!(false));
        let held = slow_producers_hold_the_cap(&state);
        let err = tiles_json(&state, &viewer_params(F_INDEX, T_INDEX + 1)).unwrap_err();
        assert_eq!(err.status, 503, "{}", err.message);
        assert!(err.message.contains(&TILE_MAX_IN_FLIGHT.to_string()));
        // A sealed tile, but never read: nothing to answer from RAM, so it must produce.
        let err = tiles_json(&state, &viewer_params(F_INDEX, T_INDEX)).unwrap_err();
        assert_eq!(err.status, 503, "{}", err.message);
        assert_eq!(state.tile_admission.hot_answers(), 0);
        drop(held);
        assert!(tiles_json(&state, &viewer_params(F_INDEX, T_INDEX)).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A batch for a drawn screen is answered whole behind slow producers** (T-581, T-573's
    /// route): a viewport of 12 addresses admits all 12 with no per-address 503, where before
    /// every member was refused once the batch's last worker met the full cap.
    #[test]
    fn a_batch_for_a_drawn_screen_admits_every_address_behind_slow_producers() {
        let dir = temp_dir("t581-batch");
        let (mut state, _, _) = state_with_history(&dir, (N as i64) + 36);
        state.tile_cache = Some(Arc::new(HotTileCache::default()));
        let screen = sealed_screen();
        for &(f, t) in &screen {
            tiles_json(&state, &viewer_params(f, t)).unwrap();
        }
        let held = slow_producers_hold_the_cap(&state);
        let addresses = screen
            .iter()
            .map(|(f, t)| format!("0.0.{f}.{t}"))
            .collect::<Vec<_>>()
            .join(",");
        let v = tiles_batch_json(
            &state,
            &params(&[
                ("cells", &N.to_string()),
                ("client", "viewer"),
                ("addresses", &addresses),
            ]),
        )
        .unwrap();
        let tiles = v["tiles"].as_array().unwrap();
        assert_eq!(tiles.len(), screen.len(), "{v}");
        let refused = tiles.iter().filter(|e| e["status"] == json!(503)).count();
        let ok = tiles.iter().filter(|e| e["status"] == json!(200)).count();
        assert_eq!(
            (ok, refused),
            (screen.len(), 0),
            "{refused} of {} batch members refused behind slow producers",
            screen.len()
        );
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two answers that differ may never share one entry: the key carries everything that decides
    /// a body. The plane spelling is the one that would be easiest to leave out and the one that
    /// would corrupt a render — a client that asked for `f16` and got JSON decodes neither.
    #[test]
    fn the_cache_key_separates_every_answer_that_differs() {
        let dir = temp_dir("cache-key");
        let (mut state, _, _) = state_with_history(&dir, (N as i64) + 36);
        state.tile_cache = Some(Arc::new(HotTileCache::default()));

        let mut packed = tile_params(F_INDEX, T_INDEX);
        packed.push(("planes".into(), "f16".into()));
        let a = tiles_json(&state, &tile_params(F_INDEX, T_INDEX)).unwrap();
        let b = tiles_json(&state, &packed).unwrap();
        assert_eq!(a["grid"]["encoding"]["planes"], json!("json"));
        assert_eq!(
            b["grid"]["encoding"]["planes"],
            json!("f16"),
            "{}",
            b["grid"]
        );
        // Both again, from cache this time, and still their own spelling.
        assert_eq!(
            tiles_json(&state, &tile_params(F_INDEX, T_INDEX)).unwrap()["grid"]["encoding"]["planes"],
            json!("json")
        );
        assert_eq!(
            tiles_json(&state, &packed).unwrap()["grid"]["encoding"]["planes"],
            json!("f16")
        );
        let s = state.tile_cache.as_ref().unwrap().stats_json();
        assert_eq!(s["entries"], json!(2), "two spellings, two entries: {s}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------------------------
    // T-573: the batch route.
    // -----------------------------------------------------------------------------------------

    fn batch_params(addresses: &str) -> Vec<(String, String)> {
        params(&[("cells", &N.to_string()), ("addresses", addresses)])
    }

    /// **A batch is as wide as its asker's share, and no wider** (T-573 follow-up).
    ///
    /// Members are answered concurrently — a sequential batch cost the sum of its members and held
    /// the live edge behind every cold tile beside it — but each still takes its own admission slot
    /// under the batch's `client`. With three of that client's four slots already held elsewhere,
    /// the batch has room for exactly one: the extra workers are refused, hand their address back
    /// and retire, so every member is still ANSWERED (on the one slot there is) rather than the
    /// batch turning its own width into a 503 per member. With all four held, nobody in the batch
    /// has a slot, and then the refusal is genuine contention and is reported per address.
    #[test]
    fn a_concurrent_batch_narrows_to_its_share_instead_of_refusing_its_own_members() {
        let dir = temp_dir("batch-share");
        let (state, _, _) = state_with_history(&dir, 8);
        let spelling = (0..4)
            .map(|i| format!("0.0.{}.{T_INDEX}", F_INDEX + i))
            .collect::<Vec<_>>()
            .join(",");
        let mut named = batch_params(&spelling);
        named.push(("client".into(), "tab".into()));

        let held: Vec<TileSlot> = (0..3)
            .map(|_| state.tile_admission.acquire("tab").unwrap())
            .collect();
        let v = tiles_batch_json(&state, &named).unwrap();
        let statuses: Vec<_> = (0..4).map(|i| v["tiles"][i]["status"].clone()).collect();
        assert_eq!(statuses, vec![json!(200); 4], "{v}");
        assert_eq!(v["truncated"], json!(false));
        let order: Vec<_> = (0..4)
            .map(|i| v["tiles"][i]["address"]["f_index"].as_i64().unwrap())
            .collect();
        assert_eq!(order, (0..4).map(|i| F_INDEX + i).collect::<Vec<_>>());

        let fourth = state.tile_admission.acquire("tab").unwrap();
        let v = tiles_batch_json(&state, &named).unwrap();
        for i in 0..4 {
            assert_eq!(v["tiles"][i]["status"], json!(503), "{v}");
        }
        drop((held, fourth));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Batching is a way to ask, not a way to summarise.** (T-573.)
    ///
    /// Every per-address fact the canvas depends on has to survive the round trip: the address
    /// itself, its independent level pair, its own coverage plane and its own `cost`. Asserted by
    /// comparing each entry against what `GET /api/tiles` answers for that address ALONE — the
    /// only difference permitted is this read's own diagnostics (`build_ms`, `in_flight`), which
    /// T-574 already had to strip before hashing a tile into an ETag for exactly this reason.
    #[test]
    fn a_batch_entry_is_what_the_single_tile_route_answers_for_that_address_alone() {
        let dir = temp_dir("batch-identity");
        let (state, _, _) = state_with_history(&dir, 8);
        // One observed address, one that nothing ever sampled, one two tiles away.
        let addrs = [F_INDEX, F_INDEX + 1, F_INDEX + 3];
        let spelling = addrs
            .iter()
            .map(|f| format!("0.0.{f}.{T_INDEX}"))
            .collect::<Vec<_>>()
            .join(",");

        let batch = tiles_batch_json(&state, &batch_params(&spelling)).unwrap();
        assert_eq!(batch["requested"], json!(3));
        assert_eq!(batch["returned"], json!(3));
        assert_eq!(batch["truncated"], json!(false));
        assert_eq!(batch["remaining"], json!([]));

        for (i, f) in addrs.iter().enumerate() {
            let e = &batch["tiles"][i];
            assert_eq!(e["status"], json!(200), "{e}");
            assert_eq!(e["address"]["f_index"], json!(*f), "{}", e["address"]);
            assert_eq!(e["address"]["t_index"], json!(T_INDEX), "{}", e["address"]);
            assert_eq!(e["address"]["level_f"], json!(0));
            assert_eq!(e["address"]["level_t"], json!(0));
            assert_eq!(
                e["address"]["spelling"],
                json!(format!("0.0.{f}.{T_INDEX}")),
                "the spelling is echoed so a follow-up request is a copy, not a re-derivation"
            );
            let alone = tiles_json(&state, &tile_params(*f, T_INDEX)).unwrap();
            let got = &e["tile"];
            for field in ["key", "extent", "axes", "grid", "coverage", "sealed"] {
                assert_eq!(
                    got[field], alone[field],
                    "{field} differs between the batch and the single-tile route at f_index {f}"
                );
            }
            // The level pair that ANSWERED, not the one addressed — the honesty tier survives.
            assert_eq!(
                got["resolution"]["answered"],
                alone["resolution"]["answered"]
            );
            assert_eq!(got["resolution"]["source"], alone["resolution"]["source"]);
            // Its own cost, per address, not one number for the set.
            assert!(got["cost"]["source_cells"].is_number(), "{}", got["cost"]);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A partial viewport is expressible, and the coverage short-circuit still short-circuits.**
    ///
    /// Three marks in one response: a tile with data, a tile nothing ever sampled, and a refusal.
    /// The unobserved one must still be answered from the coverage map — `source_cells: 0`,
    /// `chunks: 0`, `short_circuit.applied` — because a batch that made empty tiles expensive
    /// again would be a regression and not a win. There is no status for the SET: collapsing a
    /// missing tile into an empty one is the defect the coverage map exists to prevent.
    #[test]
    fn one_batch_carries_data_genuinely_unobserved_and_a_refusal_without_flattening_them() {
        let dir = temp_dir("batch-partial");
        // With an observation log: only then does the coverage map distinguish "nothing ever
        // sampled here" from "we no longer know whether we looked", and the short-circuit needs
        // the former. Without one every cell is `unknown`, which fails closed into the full read.
        let (state, _, _) = state_with_records(&dir, 8, 0);
        // The third address is above the readable ceiling, so the route refuses THAT ADDRESS with
        // its own status while the two beside it answer.
        let spelling = format!(
            "0.0.{F_INDEX}.{T_INDEX},0.0.{}.{T_INDEX},99.0.0.0",
            F_INDEX + 1
        );
        let batch = tiles_batch_json(&state, &batch_params(&spelling)).unwrap();
        assert_eq!(batch["returned"], json!(3), "{batch}");

        let (data, unobserved, refused) =
            (&batch["tiles"][0], &batch["tiles"][1], &batch["tiles"][2]);
        assert_eq!(data["status"], json!(200));
        assert!(
            data["tile"]["grid"]["observed_cells"].as_u64().unwrap() > 0,
            "the fixture's own band must carry data: {}",
            data["tile"]["grid"]
        );

        assert_eq!(unobserved["status"], json!(200), "{unobserved}");
        let u = &unobserved["tile"];
        assert_eq!(
            u["resolution"]["short_circuit"]["applied"],
            json!(true),
            "a batch must not cost an unobserved tile the generation path: {}",
            u["resolution"]["short_circuit"]
        );
        assert_eq!(u["cost"]["source_cells"], json!(0), "{}", u["cost"]);
        assert_eq!(u["cost"]["chunks"], json!(0), "{}", u["cost"]);

        assert_ne!(
            refused["status"],
            json!(200),
            "an unreadable address keeps its OWN status: {refused}"
        );
        assert!(refused["tile"].is_null(), "{refused}");
        assert!(
            refused["error"].as_str().is_some_and(|m| !m.is_empty()),
            "a refusal says why, per address: {refused}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Two caps, two answers, because they have two different causes** (T-573).
    ///
    /// Over `max_addresses` is the CALLER's doing, so it is refused with a 400 naming the cap —
    /// nothing is produced and the caller knows exactly what still needs asking for. A malformed
    /// address refuses the whole request too, rather than being skipped: a silently-dropped
    /// address is a tile left pending forever with nothing saying why.
    #[test]
    fn the_address_cap_and_a_malformed_address_are_refused_rather_than_partly_answered() {
        let dir = temp_dir("batch-caps");
        let (state, _, _) = state_with_history(&dir, 2);

        let many = (0..=TILES_BATCH_MAX_ADDRESSES)
            .map(|i| format!("0.0.{}.{T_INDEX}", F_INDEX + i as i64))
            .collect::<Vec<_>>()
            .join(",");
        let e = tiles_batch_json(&state, &batch_params(&many)).unwrap_err();
        assert_eq!(e.status, 400);
        assert!(
            e.message.contains(&TILES_BATCH_MAX_ADDRESSES.to_string()) && e.message.contains("65"),
            "the refusal names the cap and what was asked: {}",
            e.message
        );

        for bad_addr in ["0.0.1", "0.0.1.2.3", "a.0.1.2", ""] {
            let e = tiles_batch_json(&state, &batch_params(bad_addr)).unwrap_err();
            assert_eq!(e.status, 400, "{bad_addr:?} was not refused");
        }
        // The per-address parameters do not belong beside the batch; saying so beats ignoring them.
        let mut mixed = batch_params(&format!("0.0.{F_INDEX}.{T_INDEX}"));
        mixed.push(("f_index".into(), "3".into()));
        assert_eq!(tiles_batch_json(&state, &mixed).unwrap_err().status, 400);
        // T-630: `client` is a per-request fact (one request is one asker), so it rides beside the
        // addresses and every address is admitted under it, as a single-tile read would be.
        let mut named = batch_params(&format!("0.0.{F_INDEX}.{T_INDEX}"));
        named.push(("client".into(), "tab-one".into()));
        let v = tiles_batch_json(&state, &named).expect("a named batch is answered");
        assert_eq!(v["tiles"][0]["status"], json!(200), "{v}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Over the byte cap it truncates, and the remainder stays addressable.**
    ///
    /// The size cap's trigger is a property of the GRID, not of the request — a legal batch is
    /// cheap over unobserved spectrum and large over a full one — so refusing it would punish a
    /// caller for something it cannot predict. Instead the answer stops, says `truncated: true`,
    /// and lists every address it did not reach in the spelling it was asked in. And a single
    /// address over the cap on its own is still answered, or it would be unfetchable forever.
    #[test]
    fn over_the_byte_cap_the_batch_truncates_and_names_every_address_it_did_not_reach() {
        let dir = temp_dir("batch-truncate");
        let (state, _, _) = state_with_history(&dir, 8);
        let spellings: Vec<String> = (0..5)
            .map(|i| format!("0.0.{}.{T_INDEX}", F_INDEX + i))
            .collect();
        let joined = spellings.join(",");

        // A cap of one byte: the first entry alone crosses it, so exactly one tile comes back and
        // the other four are named.
        let v = tiles_batch_json_capped(&state, &batch_params(&joined), 1).unwrap();
        assert_eq!(v["requested"], json!(5));
        assert_eq!(v["returned"], json!(1), "{}", v["tiles"]);
        assert_eq!(v["truncated"], json!(true));
        assert_eq!(v["remaining"], json!(spellings[1..]), "{v}");
        assert_eq!(v["limits"]["max_response_bytes"], json!(1));
        assert_eq!(
            v["tiles"][0]["status"],
            json!(200),
            "a lone oversized address is still answered"
        );

        // Asking again for exactly what `remaining` named makes progress rather than looping.
        let again =
            tiles_batch_json_capped(&state, &batch_params(&spellings[1..].join(",")), 1).unwrap();
        assert_eq!(
            again["tiles"][0]["address"]["spelling"],
            json!(spellings[1])
        );

        // And under the real cap this same viewport is one whole answer.
        let whole = tiles_batch_json(&state, &batch_params(&joined)).unwrap();
        assert_eq!(whole["returned"], json!(5));
        assert_eq!(whole["truncated"], json!(false));
        assert_eq!(whole["remaining"], json!([]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The ticket.** A tile whose selected coverage plane is `unobserved` end to end is answered
    /// from that plane: no pyramid query, no level walk, no grid enumeration, and a body that
    /// states its one cell instead of enumerating `cells × cells` of it.
    ///
    /// Measured before this change on the same fixture shape at 256 × 256: **92 ms** of the route's
    /// own `cost.build_ms` and **2 561 726 B** on the wire, with `source_cells: 65 536` — the whole
    /// grid walked to say *nothing here*. (`crates/hk-api/tests/tile_cost.rs` is that measurement,
    /// committed so it can be re-run rather than quoted.)
    #[test]
    fn an_unobserved_tile_is_answered_from_the_coverage_map_and_no_level_is_consulted() {
        let dir = temp_dir("short-circuit");
        let (state, _, _) = state_with_records(&dir, N as i64, 0);
        // 100 tiles up the frequency axis: inside the record horizon, and no record covers it.
        let v = tiles_json(&state, &tile_params(F_INDEX + 100, T_INDEX)).unwrap();

        assert_eq!(
            v["resolution"]["short_circuit"]["applied"],
            json!(true),
            "{v}"
        );
        assert_eq!(
            v["resolution"]["short_circuit"]["selected_plane_uniform"],
            json!("unobserved"),
            "{v}"
        );
        // Nothing was read, and the answer says so rather than reporting a level that did not run.
        assert_eq!(v["cost"]["source_cells"], json!(0), "{v}");
        assert_eq!(
            v["cost"]["chunks"],
            json!(0),
            "no history lock was taken: {v}"
        );
        assert_eq!(v["resolution"]["answered"], Value::Null, "{v}");
        assert_eq!(v["resolution"]["tried"], json!([]), "{v}");
        assert_eq!(v["resolution"]["candidates"], json!([]), "{v}");

        // **The answer is "unobserved", which is not "quiet" and not "zero".** The grid states one
        // cell, and that cell has NO level — `null`, never a number, and `observed: false`.
        let g = &v["grid"];
        assert_eq!(g["uniform"]["max_db"], Value::Null, "{g}");
        assert_eq!(g["uniform"]["occupancy_max"], Value::Null, "{g}");
        assert_eq!(g["uniform"]["frames"], json!(0), "{g}");
        assert_eq!(g["uniform"]["observed"], json!(false), "{g}");
        assert_eq!(g["observed_cells"], json!(0), "{g}");
        assert_eq!(g["range_db"], Value::Null, "no scale from nothing: {g}");
        assert_eq!(
            g["cells"],
            json!(N * N),
            "the grid is still the tile's own: {g}"
        );
        // The per-cell arrays are ABSENT, not empty and not full of zeroes — an empty array would
        // read as a grid of no cells, and a zero would read as a level.
        for k in ["max_db", "occupancy_max", "coverage", "frames"] {
            assert!(g.get(k).is_none(), "grid still enumerates {k}: {g}");
        }

        // And the plane the client greys from says the same thing, in its own vocabulary.
        assert_eq!(
            v["coverage"]["planes"][v["coverage"]["selected"]["plane"].as_u64().unwrap() as usize]
                ["uniform"],
            json!("unobserved"),
            "{v}"
        );

        // The body is a constant, not a function of `cells`: the same address at 256 × 256 is the
        // same size to within the axis numbers.
        let small = serde_json::to_string(&v).unwrap().len();
        let big = serde_json::to_string(
            &tiles_json(&state, &{
                let mut p = tile_params(
                    (F_INDEX + 100) * N as i64 / TILE_CELLS as i64,
                    T_INDEX * N as i64 / TILE_CELLS as i64,
                );
                p.retain(|(k, _)| k != "cells");
                p.push(("cells".into(), TILE_CELLS.to_string()));
                p
            })
            .unwrap(),
        )
        .unwrap()
        .len();
        eprintln!(
            "T-461 short-circuit body: {small} B at {N}x{N}, {big} B at {TILE_CELLS}x{TILE_CELLS}"
        );
        assert!(
            big < small * 2,
            "a 16x larger tile must not cost 16x the body: {small} B vs {big} B"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **It fails closed, and the closing is load-bearing.**
    ///
    /// Two cases the short-circuit must refuse, and for the second one the refusal is what keeps a
    /// measurement the store holds from being served as nothing:
    ///
    /// 1. an **observed** plane — the ordinary tile, which takes the full read;
    /// 2. an **`"unknown"`** plane (T-423: a row wholly before the record horizon, where no
    ///    surviving record can say either way). `unknown` is not `unobserved`, and here the pyramid
    ///    demonstrably **holds data** for exactly those cells. Had the short-circuit fired on
    ///    `unknown`, this tile would have been served as an empty grid over a measured band — which
    ///    is precisely the defect the ticket named as worse than being slow.
    #[test]
    fn the_short_circuit_refuses_an_observed_plane_and_refuses_unknown_over_data_the_store_holds() {
        // 1. Observed: the full read runs and the tile carries its measurements.
        let dir = temp_dir("closed-observed");
        let (state, _, _) = state_with_records(&dir, N as i64, 0);
        let v = tiles_json(&state, &tile_params(F_INDEX, T_INDEX)).unwrap();
        assert_eq!(
            v["resolution"]["short_circuit"]["applied"],
            json!(false),
            "{v}"
        );
        assert_eq!(
            v["resolution"]["short_circuit"]["selected_plane_uniform"],
            json!("observed"),
            "{v}"
        );
        assert!(v["cost"]["source_cells"].as_u64().unwrap() > 0, "{v}");
        assert!(v["grid"]["observed_cells"].as_u64().unwrap() > 0, "{v}");
        assert!(
            v["grid"]["max_db"].is_array(),
            "the full path enumerates: {v}"
        );
        assert!(v["grid"].get("uniform").is_none(), "{v}");
        let _ = std::fs::remove_dir_all(&dir);

        // 2. Unknown, over a band the pyramid holds frames for. The record horizon is an hour after
        //    the tile, so no surviving record can say whether we looked — and the measurement is
        //    still there to be served.
        let dir = temp_dir("closed-unknown");
        let (state, _, _) = state_with_records(&dir, N as i64, 3600);
        let v = tiles_json(&state, &tile_params(F_INDEX, T_INDEX)).unwrap();
        assert_eq!(
            v["resolution"]["short_circuit"]["selected_plane_uniform"],
            json!("unknown"),
            "the fixture must actually produce a uniformly UNKNOWN plane: {}",
            v["coverage"]
        );
        assert_eq!(
            v["resolution"]["short_circuit"]["applied"],
            json!(false),
            "`unknown` is not `unobserved`, and short-circuiting on it would grey measured \
             spectrum: {v}"
        );
        // The load-bearing half: there really is data here, so the refusal saved something.
        assert!(
            v["grid"]["observed_cells"].as_u64().unwrap() > 0,
            "the mutation control is vacuous unless the store holds data here: {}",
            v["grid"]["observed_cells"]
        );
        assert!(v["grid"]["max_db"].is_array(), "{v}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The equivalence.** The constants [`unobserved_grid_json`] serves are exactly what a real
    /// store read produces for the same address — so the short-circuit is a cheaper *spelling* of
    /// the full path's answer, not a second answer about the same tile.
    #[test]
    fn an_unobserved_tile_read_produces_exactly_the_constants_the_short_circuit_serves() {
        let dir = temp_dir("equivalent");
        let (state, _, _) = state_with_history(&dir, N as i64);
        let g = crate::http::with_history(&state, |p| Ok(p.geometry().clone())).unwrap();
        let key = parse_key(&g, &tile_params(F_INDEX + 100, T_INDEX)).unwrap();
        let r = tile_read(&state, TileStore::Main, &key).unwrap();
        // What the full read actually produced over never-sampled spectrum.
        assert_eq!(r.grid.observed_cells, 0);
        assert_eq!(r.grid.range_db, None);
        assert_eq!(r.grid.unit, hk_model::PowerUnit::Dbfs);
        assert!(
            r.grid.cells.iter().all(|c| {
                c.sources == 0 && c.frames == 0 && c.coverage == 0.0 && !c.max_db.is_finite()
            }),
            "the full read must be uniformly UNOBSERVED for this comparison to mean anything"
        );
        // …and the short form says the same, field for field.
        let short = unobserved_grid_json(&key, Planes::Json);
        let full = grid_json(&r.grid, Planes::Json);
        assert_eq!(short["unit"], full["unit"]);
        assert_eq!(short["cells"], full["cells"]);
        assert_eq!(short["observed_cells"], full["observed_cells"]);
        assert_eq!(short["range_db"], full["range_db"]);
        assert_eq!(short["semantics"], full["semantics"]);
        assert_eq!(short["encoding"], full["encoding"]);
        assert_eq!(short["uniform"], uniform_cell_json(&r.grid.cells[0]));
        // The mutation: had the uniform cell been written as zeroes rather than as absences, it
        // would differ — which is what makes the equality above an assertion and not a tautology.
        let zeroed = hk_store::OverviewCell {
            max_db: 0.0,
            occupancy_max: 0.0,
            ..hk_store::OverviewCell::UNOBSERVED
        };
        assert_ne!(
            short["uniform"],
            uniform_cell_json(&zeroed),
            "a level of zero and the absence of a level must not serialise the same"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The cost, measured through the route, before and after, in one run.**
    ///
    /// The short-circuit's floor is the coverage rasterisation itself — which is the point: it
    /// reads the same plane the renderer greys from, so that work is the answer rather than
    /// overhead. What disappears is the `O(cells²)` pyramid walk on top of it.
    #[test]
    fn the_short_circuit_is_measured_against_the_read_it_replaces() {
        let dir = temp_dir("sc-cost");
        let (state, _, _) = state_with_records(&dir, TILE_CELLS as i64, 0);
        let g = crate::http::with_history(&state, |p| Ok(p.geometry().clone())).unwrap();
        let n = 8;
        let time = |f_index: i64| -> f64 {
            let q = tile_params(f_index, T_INDEX);
            let _ = tiles_json(&state, &q).unwrap();
            let started = std::time::Instant::now();
            for _ in 0..n {
                std::hint::black_box(tiles_json(&state, &q).unwrap());
            }
            started.elapsed().as_secs_f64() * 1e3 / n as f64
        };
        // The same address, read the long way, so the comparison is against this machine and this
        // fixture rather than against a number from another run.
        let key = parse_key(&g, &tile_params(F_INDEX + 100, T_INDEX)).unwrap();
        let started = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(
                tile_read(&state, TileStore::Main, &key)
                    .unwrap()
                    .source_cells,
            );
        }
        let full_read_ms = started.elapsed().as_secs_f64() * 1e3 / n as f64;
        let short_ms = time(F_INDEX + 100);
        let observed_ms = time(F_INDEX);
        eprintln!(
            "T-461 at {N}x{N}: pyramid read alone over never-sampled spectrum {full_read_ms:.2} ms; \
             whole short-circuited answer {short_ms:.2} ms; whole observed answer {observed_ms:.2} ms"
        );
        assert!(
            short_ms < observed_ms,
            "the short-circuited answer must be cheaper than the full one: {short_ms:.2} vs {observed_ms:.2} ms"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The number this route is designed against.** T-437 measured rendering at p95 2.2 ms for
    /// 48 panes and tile *production* at ~500 ms per tile over HTTP — three orders of magnitude
    /// apart — so production is where the budget goes. Measured here through the real store and
    /// the real fold; the assertion is a generous ceiling, the print is the measurement.
    #[test]
    fn tile_production_cost_is_measured_and_stays_inside_the_screen_budget() {
        let dir = temp_dir("cost");
        let (state, _, _) = state_with_history(&dir, TILE_CELLS as i64);
        let g = crate::http::with_history(&state, |p| Ok(p.geometry().clone())).unwrap();
        let n = 16;
        let mut worst: f64 = 0.0;
        for &cells in &[N, TILE_CELLS] {
            // The canvas's own unit is 256 x 256 (docs/16 §6.2); N is the same read an eighth of
            // the area, so the pair says whether the cost is in the fold or in the fixed overhead.
            let t_index = T_INDEX * N as i64 / cells as i64;
            let f_index = F_INDEX * N as i64 / cells as i64;
            let started = std::time::Instant::now();
            for i in 0..n {
                let key = parse_key(
                    &g,
                    &params(&[
                        ("level_f", "0"),
                        ("level_t", "0"),
                        ("f_index", &(f_index + i % 3).to_string()),
                        ("t_index", &t_index.to_string()),
                        ("cells", &cells.to_string()),
                    ]),
                )
                .unwrap();
                std::hint::black_box(
                    tile_read(&state, TileStore::Main, &key)
                        .unwrap()
                        .grid
                        .observed_cells,
                );
            }
            let mean_ms = started.elapsed().as_secs_f64() * 1e3 / n as f64;
            eprintln!(
                "T-438 tile production: {mean_ms:.2} ms mean over {n} tiles of {cells}x{cells} cells"
            );
            worst = worst.max(mean_ms);
        }
        assert!(
            worst < 250.0,
            "tile production regressed to {worst:.1} ms/tile"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T-519's fixture: band X (tile `F_INDEX`) holds a -60 dB carrier over noise for the first
    /// `x_secs` of tile `T_INDEX`, then is departed; band Y (tile `F_INDEX + 2`) stays observed for
    /// two whole tiles, so the store's data edge is the end of tile `T_INDEX + 1`. Tile
    /// `F_INDEX + 1` is never observed at all.
    fn state_swept_then_departed(dir: &std::path::Path, x_secs: i64) -> (ApiState, i64) {
        let mut p = hk_store::Pyramid::open(dir, PyramidConfig::default()).unwrap();
        let g = p.geometry().clone();
        let (t_cell, f_cell) = (g.levels[0].t_cell_ns, g.levels[0].f_cell_hz);
        let t0 = T_INDEX * t_cell * N as i64;
        const NB: usize = 128;
        let bin_hz = f_cell * N as f64 / NB as f64;
        for k in 0..2 * N as i64 {
            let mut psd = [1e-12f32; NB];
            psd[40] = 1e-6;
            let bands = if k < x_secs {
                &[0i64, 2][..]
            } else {
                &[2i64][..]
            };
            for &b in bands {
                p.ingest(&hk_store::history::FrameInput::new(
                    Timestamp::from_unix_nanos(t0 + k * t_cell),
                    t_cell,
                    (F_INDEX + b) as f64 * f_cell * N as f64,
                    bin_hz,
                    hk_model::PowerUnit::Dbfs,
                    &psd,
                ))
                .unwrap();
            }
        }
        let state = ApiState {
            history: Some(Arc::new(std::sync::Mutex::new(p))),
            ..ApiState::default()
        };
        (state, t0)
    }

    /// Expands the `shadow` block's runs into a per-cell
    /// `(last_db, last_t_s, source f cell, fill)` plane; the source's frequency cell is the tile's
    /// own for a value this tile holds, and `fill` is the run's direction as the wire spells it
    /// (T-527) — read through the `fills` legend, never assumed from the code.
    fn shadow_plane(v: &Value) -> Vec<Option<(f64, f64, f64, String)>> {
        let sh = &v["shadow"];
        assert_eq!(sh["encoding"], json!("column-runs"), "{sh}");
        let n = v["extent"]["nf"].as_u64().unwrap() as usize;
        let tile_f_cell = v["extent"]["f_cell_hz"].as_f64().unwrap();
        let mut out = vec![None; n * n];
        let arr = |k: &str| sh[k].as_array().unwrap().clone();
        let (f, row, rows, db, t, src, fill) = (
            arr("f"),
            arr("row"),
            arr("rows"),
            arr("last_db"),
            arr("last_t_s"),
            arr("src"),
            arr("fill"),
        );
        let fills = arr("fills");
        assert_eq!(fills, vec![json!("forward"), json!("backward")], "{sh}");
        assert_eq!(sh["runs"].as_u64().unwrap() as usize, f.len());
        for a in [&row, &rows, &db, &t, &src, &fill] {
            assert_eq!(a.len(), f.len(), "parallel arrays: {sh}");
        }
        for i in 0..f.len() {
            let source = &sh["sources"][src[i].as_u64().unwrap() as usize];
            assert!(source.is_object(), "src indexes `sources`: {sh}");
            let f_cell = source["f_cell_hz"].as_f64().unwrap_or(tile_f_cell);
            let dir = fills[fill[i].as_u64().expect("a fill code") as usize]
                .as_str()
                .expect("`fill` indexes `fills`")
                .to_string();
            let c = f[i].as_u64().unwrap() as usize;
            let r0 = row[i].as_u64().unwrap() as usize;
            for r in r0..r0 + rows[i].as_u64().unwrap() as usize {
                assert!(out[r * n + c].is_none(), "runs overlap at ({r}, {c})");
                out[r * n + c] = Some((
                    db[i].as_f64().unwrap(),
                    t[i].as_f64().unwrap(),
                    f_cell,
                    dir.clone(),
                ));
            }
        }
        out
    }

    /// **T-519, the user's three states.** Swept then departed = a shadow carrying the value last
    /// seen, with when; never swept = NO shadow; observed now = its own value, never a shadow.
    #[test]
    fn a_departed_band_carries_its_last_known_value_and_a_never_swept_one_carries_none() {
        let dir = temp_dir("shadow");
        let half = N as i64 / 2;
        let h = half as usize;
        let (state, t0) = state_swept_then_departed(&dir, half);
        let (n, t0_s) = (N, t0 as f64 / 1e9);

        // The tile band X was seen in, for its first half.
        let here = tiles_json(&state, &tile_params(F_INDEX, T_INDEX)).unwrap();
        let grid = here["grid"]["max_db"].as_array().unwrap().clone();
        let plane = shadow_plane(&here);
        for f in 0..n {
            let seen = grid[(h - 1) * n + f].as_f64().expect("row half-1 measured");
            for r in 0..n {
                if r < h {
                    // Observed now: the grid's value stands and NO shadow is laid over it.
                    assert!(grid[r * n + f].is_number(), "({r}, {f})");
                    assert_eq!(
                        plane[r * n + f],
                        None,
                        "a shadow over a measurement ({r}, {f})"
                    );
                } else {
                    // Departed part-way down this tile: the value it was last seen with, and when.
                    assert!(grid[r * n + f].is_null());
                    let (db, t, _, fill) = plane[r * n + f]
                        .clone()
                        .expect("shadow below the departure");
                    assert_eq!(fill, "forward", "a departure carries FORWARD ({r}, {f})");
                    assert_eq!((db, t), (seen, t0_s + half as f64), "({r}, {f})");
                }
            }
        }

        // The next tile up: band X was never seen in it, and every cell carries what it was last
        // seen with — the SAME value the tile below measured (T-315: assert the value), at the
        // resolution of the level that held it, which `sources` states: a source cell k tile
        // columns wide carries the max-hold of those k columns, replicated (T-334's direction).
        let next = tiles_json(&state, &tile_params(F_INDEX, T_INDEX + 1)).unwrap();
        assert_eq!(next["grid"]["observed_cells"], json!(0), "{}", next["grid"]);
        let plane = shadow_plane(&next);
        let carrier = grid[(h - 1) * n + 20].as_f64().unwrap();
        let noise = grid[(h - 1) * n].as_f64().unwrap();
        assert!(
            carrier > noise + 30.0,
            "the fixture's carrier: {carrier} vs {noise}"
        );
        let tile_f_cell = next["extent"]["f_cell_hz"].as_f64().unwrap();
        for r in 0..n {
            for f in 0..n {
                let (db, t, f_cell, fill) = plane[r * n + f]
                    .clone()
                    .expect("every cell of the departed band");
                assert_eq!(fill, "forward", "({r}, {f})");
                let k = (f_cell / tile_f_cell).round().max(1.0) as usize;
                let g = f / k * k;
                let expect = (g..g + k)
                    .map(|c| grid[(h - 1) * n + c].as_f64().unwrap())
                    .fold(f64::NEG_INFINITY, f64::max);
                assert_eq!(db, expect, "({r}, {f}) from a {f_cell} Hz source");
                let (lo, hi) = (t0_s + half as f64, t0_s + n as f64);
                assert!(t >= lo && t <= hi, "last seen {t} outside [{lo}, {hi}]");
            }
        }
        assert!(
            plane
                .iter()
                .any(|c| c.as_ref().is_some_and(|(db, ..)| *db == carrier))
        );
        assert_eq!(next["shadow"]["search"]["columns_found"], json!(n));
        // T-911: and that level is THE TILE'S OWN, so the carried value is exactly the cell of the
        // band's last live row, column for column — never a max-hold over some other box, which is
        // the colour change the user saw at the first tile boundary after leaving a band.
        let before: Vec<&Value> = next["shadow"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["from"] == json!("before-tile"))
            .collect();
        assert!(!before.is_empty(), "{}", next["shadow"]);
        for s in &before {
            assert_eq!(s["search"], json!("own-level"), "{s}");
            assert_eq!(s["f_cell_hz"].as_f64(), Some(tile_f_cell), "{s}");
        }
        for f in 0..n {
            let (db, ..) = plane[f].clone().expect("row 0 carries");
            assert_eq!(db, grid[(h - 1) * n + f].as_f64().unwrap(), "column {f}");
        }
        assert_eq!(
            next["shadow"]["search"]["own_level"]["columns_used"],
            json!(n),
            "{}",
            next["shadow"]["search"]
        );

        // Never swept: NO shadow anywhere, and the search says it found nothing — grey stays grey.
        let never = tiles_json(&state, &tile_params(F_INDEX + 1, T_INDEX + 1)).unwrap();
        assert_eq!(never["shadow"]["runs"], json!(0), "{}", never["shadow"]);
        assert_eq!(never["shadow"]["search"]["columns_found"], json!(0));
        assert!(shadow_plane(&never).iter().all(Option::is_none));

        // Past the data edge nothing is carried: every row of the tile after the newest frame is
        // the future, so it carries nothing even over band X.
        let future = tiles_json(&state, &tile_params(F_INDEX, T_INDEX + 2)).unwrap();
        assert_eq!(future["shadow"]["runs"], json!(0), "{}", future["shadow"]);

        eprintln!(
            "T-519 shadow: departed tile {} runs, {} B shadow block, search {} ms / {} cells",
            next["shadow"]["runs"],
            next["shadow"].to_string().len(),
            next["shadow"]["search"]["build_ms"],
            next["shadow"]["search"]["source_cells"],
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T-881's fixture — the fog-of-war scene with the fold **trailing** capture, as it does on a
    /// live server. Band X (tile `F_INDEX`) is swept for the first `half` rows of tile `T_INDEX`
    /// (a sealed dwell) and then departed; the radio moves to band Y (tile `F_INDEX + 2`, the dwell
    /// in flight), whose tune record reaches `record` rows past `t0` — but the spectrum history has
    /// folded Y's frames only as far as `folded` rows. Tile `F_INDEX + 1` is never observed.
    fn state_departed_with_fold_lag(
        dir: &std::path::Path,
        half: i64,
        folded: i64,
        record: i64,
    ) -> (ApiState, i64, i64) {
        let mut p = hk_store::Pyramid::open(dir, PyramidConfig::default()).unwrap();
        let g = p.geometry().clone();
        let (t_cell, f_cell) = (g.levels[0].t_cell_ns, g.levels[0].f_cell_hz);
        let band = |b: i64| (F_INDEX + b) as f64 * f_cell * N as f64;
        let t0 = T_INDEX * t_cell * N as i64;
        const NB: usize = 128;
        let bin_hz = f_cell * N as f64 / NB as f64;
        for k in 0..folded {
            let mut psd = [1e-12f32; NB];
            psd[40] = 1e-6;
            let bands = if k < half {
                &[0i64, 2][..]
            } else {
                &[2i64][..]
            };
            for &b in bands {
                p.ingest(&hk_store::history::FrameInput::new(
                    Timestamp::from_unix_nanos(t0 + k * t_cell),
                    t_cell,
                    band(b),
                    bin_hz,
                    hk_model::PowerUnit::Dbfs,
                    &psd,
                ))
                .unwrap();
            }
        }
        let store = hk_store::observation::ObservationStore::open(
            hk_store::observation::ObservationLogConfig::new(dir.join("observations")),
        )
        .unwrap();
        let width = f_cell * N as f64;
        store.append(&dwell(band(0), band(0) + width, t0, t0 + half * t_cell));
        store.flush();
        let hk_model::attention::observation::ObservationRecord::Dwell(open) = dwell(
            band(2),
            band(2) + width,
            t0 + half * t_cell,
            t0 + record * t_cell,
        ) else {
            unreachable!("dwell builds a dwell")
        };
        store.note_open_dwell(open);
        let state = ApiState {
            history: Some(Arc::new(std::sync::Mutex::new(p))),
            observations: Some(store),
            ..ApiState::default()
        };
        (state, t0, t_cell)
    }

    /// The selected coverage plane of a tile answer, one state name per cell.
    fn selected_states(v: &Value) -> Vec<String> {
        let states = v["coverage"]["states"].as_array().unwrap();
        let sel = v["coverage"]["selected"]["plane"].as_u64().unwrap() as usize;
        let runs = v["coverage"]["planes"][sel]["runs"].as_array().unwrap();
        let mut plane = Vec::with_capacity(N * N);
        for pair in runs.chunks(2) {
            let s = states[pair[0].as_u64().unwrap() as usize].as_str().unwrap();
            for _ in 0..pair[1].as_u64().unwrap() {
                plane.push(s.to_string());
            }
        }
        assert_eq!(plane.len(), N * N, "{}", v["coverage"]);
        plane
    }

    /// The cells a client following docs/api.md draws as **THE grey** from this answer: drawn at
    /// all (the row starts before `coverage.horizon.as_of_s`, or there is no horizon — T-532), the
    /// selected plane says `unobserved`, and no `shadow` run covers the cell (T-520).
    fn drawn_grey(v: &Value) -> Vec<(usize, usize)> {
        let plane = selected_states(v);
        let shade = shadow_plane(v);
        let (t0_s, dt) = (
            v["extent"]["t0_s"].as_f64().unwrap(),
            v["extent"]["t_cell_s"].as_f64().unwrap(),
        );
        let as_of = v["coverage"]["horizon"]["as_of_s"].as_f64();
        (0..N * N)
            .map(|i| (i / N, i % N))
            .filter(|&(r, f)| {
                as_of.is_none_or(|a| t0_s + r as f64 * dt < a)
                    && plane[r * N + f] == "unobserved"
                    && shade[r * N + f].is_none()
            })
            .collect()
    }

    /// **T-881: a departed band's newest rows are its shadow, never THE grey.**
    ///
    /// Observed by the fog-of-war guard: the top of a departed band's pane was plain grey — no
    /// pending, no stand-ins — over 10–60 % of it. Two things made that strip, both here:
    ///
    /// 1. **The shadow stopped at the store's newest FOLDED frame** (`shadow.edge_s`), and the fold
    ///    trails capture. The rows between it and the newest instant the tune record reaches are
    ///    time the radio spent on another band — the band's last-known value is exactly as true of
    ///    them — and they carried no run.
    /// 2. **The horizon was read over this band alone.** A tile wholly after the departure has no
    ///    record of *this* band in it, so it named no `as_of_s` and was drawn as served — grey — for
    ///    as long as a client kept it; a tile straddling the departure named the departure itself,
    ///    so every row after it stayed the pane's pending ground for good, never the shadow.
    ///
    /// RED before T-881: tile `T_INDEX + 1` of band X has no horizon and its rows from the fold
    /// edge up are drawn grey; tile `T_INDEX`'s horizon is the departure.
    #[test]
    fn a_departed_band_reads_as_shadow_up_to_the_record_reach_not_grey_past_the_fold_edge() {
        let dir = temp_dir("t881-fold-lag");
        let (half, folded, record) = (
            N as i64 / 2,
            N as i64 + N as i64 / 4,
            N as i64 + 3 * N as i64 / 4,
        );
        let (state, t0, t_cell) = state_departed_with_fold_lag(&dir, half, folded, record);
        let s_of = |rows: i64| (t0 + rows * t_cell) as f64 / 1e9;
        let rows_in_next = |rows: i64| (rows - N as i64) as usize;

        // The tile wholly after the departure: band X's newest rows.
        let next = tiles_json(&state, &tile_params(F_INDEX, T_INDEX + 1)).unwrap();
        assert_eq!(
            next["shadow"]["edge_s"],
            json!(s_of(folded)),
            "{}",
            next["shadow"]
        );
        assert_eq!(
            next["coverage"]["horizon"]["as_of_s"],
            json!(s_of(record)),
            "the answer's evidence reaches as far as the radio's record does, over ANY band — the \
             radio was on band Y, so band X was unobserved up to there: {}",
            next["coverage"]["horizon"]
        );
        assert_eq!(
            next["shadow"]["reach_s"],
            json!(s_of(record)),
            "{}",
            next["shadow"]
        );
        let grey = drawn_grey(&next);
        assert!(
            grey.is_empty(),
            "departed band X draws {} cells as THE grey (rows {:?}..): swept then left is the \
             last-known shadow, never 'never observed'",
            grey.len(),
            grey.first()
        );
        let plane = shadow_plane(&next);
        for f in 0..N {
            for r in 0..N {
                assert_eq!(
                    plane[r * N + f].is_some(),
                    r < rows_in_next(record),
                    "band X ({r}, {f}): shadow up to the record's reach (row {}), and none in the \
                     future past it",
                    rows_in_next(record)
                );
            }
        }

        // The tile the departure falls in: drawn to its end, the rows after the departure shadow.
        let here = tiles_json(&state, &tile_params(F_INDEX, T_INDEX)).unwrap();
        let t1_s = here["extent"]["t1_s"].as_f64().unwrap();
        assert_eq!(
            here["coverage"]["horizon"]["as_of_s"],
            json!(t1_s),
            "the record reaches past this tile, so nothing in it is left as 'not reached yet' — \
             before T-881 the horizon was the departure, and the rows after it never drew: {}",
            here["coverage"]["horizon"]
        );
        assert!(drawn_grey(&here).is_empty(), "{:?}", drawn_grey(&here));

        // Band Y past the fold edge: observed, its frames not folded yet — NOT a shadow's to cover.
        let y = tiles_json(&state, &tile_params(F_INDEX + 2, T_INDEX + 1)).unwrap();
        let y_states = selected_states(&y);
        assert!(
            (0..rows_in_next(record)).all(|r| y_states[r * N] == "observed"),
            "{:?}",
            &y_states[..N]
        );
        assert_eq!(
            y["shadow"]["runs"],
            json!(0),
            "a band the radio is on carries no shadow, folded or not: {}",
            y["shadow"]
        );

        // Never swept: no shadow at all, and grey only as far as the record reaches.
        let never = tiles_json(&state, &tile_params(F_INDEX + 1, T_INDEX + 1)).unwrap();
        assert_eq!(never["shadow"]["runs"], json!(0), "{}", never["shadow"]);
        assert_eq!(never["coverage"]["horizon"]["as_of_s"], json!(s_of(record)));
        assert_eq!(drawn_grey(&never).len(), rows_in_next(record) * N);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T-527's fixture: band X (tile `F_INDEX`) is **first ever seen** `x_from` rows down tile
    /// `T_INDEX` — it has no past whatever — and is observed from there on. Band Y
    /// (tile `F_INDEX + 2`) is observed throughout both tiles, so the store's edge is past
    /// `T_INDEX` and tile `F_INDEX + 1` is still never observed at all.
    fn state_arrived_mid_tile(dir: &std::path::Path, x_from: i64) -> (ApiState, i64) {
        let mut p = hk_store::Pyramid::open(dir, PyramidConfig::default()).unwrap();
        let g = p.geometry().clone();
        let (t_cell, f_cell) = (g.levels[0].t_cell_ns, g.levels[0].f_cell_hz);
        let t0 = T_INDEX * t_cell * N as i64;
        const NB: usize = 128;
        let bin_hz = f_cell * N as f64 / NB as f64;
        for k in 0..2 * N as i64 {
            let mut psd = [1e-12f32; NB];
            psd[40] = 1e-6;
            let bands = if k >= x_from {
                &[0i64, 2][..]
            } else {
                &[2i64][..]
            };
            for &b in bands {
                p.ingest(&hk_store::history::FrameInput::new(
                    Timestamp::from_unix_nanos(t0 + k * t_cell),
                    t_cell,
                    (F_INDEX + b) as f64 * f_cell * N as f64,
                    bin_hz,
                    hk_model::PowerUnit::Dbfs,
                    &psd,
                ))
                .unwrap();
            }
        }
        (
            ApiState {
                history: Some(Arc::new(std::sync::Mutex::new(p))),
                ..ApiState::default()
            },
            t0,
        )
    }

    /// **T-527, on the wire.** A column observed only part-way down the view leaves no grey gap
    /// above its samples: the stretch before its **first-ever** sample carries that sample, marked
    /// `backward`, and every row after a sample carries the nearest past one, marked `forward`.
    /// A column never observed at all still carries **no run** — that is what keeps grey meaning
    /// *we never looked*.
    #[test]
    fn the_rows_above_a_columns_first_ever_sample_carry_it_backward_and_say_so() {
        let dir = temp_dir("shadow-backward");
        let half = N as i64 / 2;
        let h = half as usize;
        let (state, t0) = state_arrived_mid_tile(&dir, half);
        let (n, t0_s) = (N, t0 as f64 / 1e9);

        let here = tiles_json(&state, &tile_params(F_INDEX, T_INDEX)).unwrap();
        let grid = here["grid"]["max_db"].as_array().unwrap().clone();
        let sh = &here["shadow"];
        // Nothing older than this tile exists for band X: the whole head gap is the backward fill's
        // to answer, and if the search had found something this would be a different test.
        assert_eq!(
            sh["search"]["columns_found"],
            json!(0),
            "the fixture must have no past for band X: {sh}"
        );
        assert_eq!(sh["backward_runs"], json!(n), "one per column: {sh}");
        let plane = shadow_plane(&here);
        for f in 0..n {
            // The first-ever sample: the value in the first row the grid measures.
            let first = grid[h * n + f].as_f64().expect("row half measured");
            for r in 0..n {
                if r >= h {
                    assert!(grid[r * n + f].is_number(), "({r}, {f})");
                    assert_eq!(
                        plane[r * n + f],
                        None,
                        "a shadow over a measurement ({r}, {f})"
                    );
                    continue;
                }
                assert!(grid[r * n + f].is_null(), "({r}, {f}) must be a gap");
                let (db, t, _, fill) = plane[r * n + f]
                    .clone()
                    .unwrap_or_else(|| panic!("row {r} of column {f} left grey"));
                assert_eq!(fill, "backward", "({r}, {f})");
                assert_eq!(db, first, "the first-ever sample's value ({r}, {f})");
                // First seen at the START of that sample's cell — which lies AFTER these rows.
                // That is the whole difference from a forward run, and it is on the wire.
                assert_eq!(t, t0_s + half as f64, "({r}, {f})");
                assert!(
                    t > t0_s + r as f64,
                    "a backward run's instant is after its rows"
                );
            }
        }
        // The same tile's source table is the tile's own grid, not a store level: a backward fill
        // can only ever be a value this tile holds.
        for i in 0..sh["runs"].as_u64().unwrap() as usize {
            let s = &sh["sources"][sh["src"][i].as_u64().unwrap() as usize];
            assert_eq!(s["from"], json!("this-tile"), "{sh}");
        }

        // **Never observed stays grey.** The next band over has no sample and nothing older, so it
        // carries NO run — there is no first-ever sample to read back from, and the backward fill
        // must never invent one.
        let never = tiles_json(&state, &tile_params(F_INDEX + 1, T_INDEX)).unwrap();
        assert_eq!(never["shadow"]["runs"], json!(0), "{}", never["shadow"]);
        assert_eq!(never["shadow"]["backward_runs"], json!(0));
        assert_eq!(never["shadow"]["f"], json!([]), "{}", never["shadow"]);
        assert_eq!(never["shadow"]["search"]["columns_found"], json!(0));
        assert!(shadow_plane(&never).iter().all(Option::is_none));

        // And the tile BELOW (later than) the arrival carries the band forward, as before: the
        // backward fill is the head's rule only, never a second way to answer an ordinary gap.
        let below = tiles_json(&state, &tile_params(F_INDEX, T_INDEX + 1)).unwrap();
        assert_eq!(
            below["shadow"]["backward_runs"],
            json!(0),
            "{}",
            below["shadow"]
        );

        eprintln!(
            "T-527: {} runs ({} backward), {} B shadow block, search {} ms / {} cells",
            sh["runs"],
            sh["backward_runs"],
            sh.to_string().len(),
            sh["search"]["build_ms"],
            sh["search"]["source_cells"],
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// **binary16 is a re-spelling of the value, not a new value.** (T-533)
    ///
    /// Every assertion here is about a claim the wire makes: that `type: "f16"` means IEEE 754
    /// binary16 (so the bit patterns are the standard's, not a near-miss of it), that `absent:
    /// "nan"` is kept (a NaN that became a zero or an infinity would invent a level — C26), and
    /// that the error introduced is bounded by half an ulp, which for the dB range this route
    /// serves is well under a tenth of a decibel.
    #[test]
    fn f16_is_ieee_binary16_and_keeps_absence_absent() {
        // The standard's own landmarks, bit for bit.
        assert_eq!(f16_bits(0.0), 0x0000);
        assert_eq!(f16_bits(-0.0), 0x8000);
        assert_eq!(f16_bits(1.0), 0x3c00);
        assert_eq!(f16_bits(-2.0), 0xc000);
        assert_eq!(f16_bits(65504.0), 0x7bff, "the largest finite binary16");
        assert_eq!(f16_bits(65536.0), 0x7c00, "past the range: infinity");
        assert_eq!(f16_bits(f32::INFINITY), 0x7c00);
        assert_eq!(f16_bits(6.103_515_6e-5), 0x0400, "smallest normal");
        assert_eq!(f16_bits(5.960_464_5e-8), 0x0001, "smallest subnormal");
        assert_eq!(f16_bits(1e-9), 0x0000, "below the smallest subnormal");
        // Absence stays absence. The exponent is all ones AND the mantissa is non-zero, which is
        // what makes it a NaN rather than the infinity next door.
        for nan in [f32::NAN, -f32::NAN] {
            let b = f16_bits(nan);
            assert_eq!(b & 0x7c00, 0x7c00, "{b:#06x} is not a NaN or infinity");
            assert_ne!(b & 0x03ff, 0, "{b:#06x} became an infinity, not a NaN");
        }
        // Round to nearest EVEN at the tie, not away from zero: 2049 sits exactly between two
        // representable values (2048 and 2050) and must land on the even one.
        assert_eq!(f16_bits(2049.0), f16_bits(2048.0));
        assert_eq!(f16_bits(2051.0), f16_bits(2052.0));
        // The error bound, over the dB range this route actually serves.
        let back = |b: u16| {
            let s = if b >> 15 == 1 { -1.0f32 } else { 1.0 };
            let (e, m) = ((b >> 10) & 0x1f, (b & 0x3ff) as f32);
            match e {
                0 => s * m * 2f32.powi(-24),
                31 => f32::NAN,
                _ => s * (m + 1024.0) * 2f32.powi(e as i32 - 25),
            }
        };
        let mut worst = 0.0f32;
        let mut x = -160.0f32;
        while x <= 0.0 {
            worst = worst.max((back(f16_bits(x)) - x).abs());
            x += 0.013;
        }
        assert!(
            worst < 0.07,
            "binary16 costs {worst} dB over [-160, 0] dBFS, which is more than the R16F texture \
             this plane is uploaded into would have cost anyway"
        );
    }

    #[test]
    fn base64_is_the_standard_alphabet_with_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }

    /// **The two spellings are the same grid**, cell for cell, on a real store read — which is the
    /// only thing that makes `?planes=f16` a representation choice rather than a second answer.
    #[test]
    fn the_f16_plane_carries_the_same_values_the_json_array_does() {
        let dir = temp_dir("planes-f16");
        let (state, _, _) = state_with_records(&dir, N as i64, 0);
        let q = tile_params(F_INDEX, T_INDEX);
        let plain = tiles_json(&state, &q).unwrap();
        let mut packed_q = q.clone();
        packed_q.push(("planes".into(), "f16".into()));
        let packed = tiles_json(&state, &packed_q).unwrap();

        assert_eq!(plain["grid"]["encoding"]["planes"], json!("json"));
        assert_eq!(packed["grid"]["encoding"]["planes"], json!("f16"));
        // Absent, not empty, in each direction.
        assert!(plain["grid"]["planes"].is_null(), "{}", plain["grid"]);
        assert!(packed["grid"]["max_db"].is_null(), "{}", packed["grid"]);

        let json_cells = plain["grid"]["max_db"].as_array().expect("max_db array");
        let plane = &packed["grid"]["planes"]["max_db"];
        assert_eq!(plane["type"], json!("f16"));
        assert_eq!(plane["byte_order"], json!("little-endian"));
        assert_eq!(plane["transfer"], json!("base64"));
        assert_eq!(plane["absent"], json!("nan"));
        assert_eq!(plane["cells"], json!(json_cells.len()));
        assert_eq!(plane["bytes"], json!(json_cells.len() * 2));
        assert_eq!(
            plane["scale"],
            plain["grid"]["semantics"]["series"]["max_db"]["scale"]
        );

        // Decode the plane the way the client does, and compare.
        let bytes = decode_base64(plane["data"].as_str().expect("data"));
        assert_eq!(bytes.len(), json_cells.len() * 2);
        let mut observed = 0usize;
        for (i, cell) in json_cells.iter().enumerate() {
            let bits = u16::from_le_bytes([bytes[i * 2], bytes[i * 2 + 1]]);
            let finite = bits & 0x7c00 != 0x7c00;
            match cell.as_f64() {
                None => assert!(
                    !finite,
                    "cell {i} is null in JSON and a number in the plane"
                ),
                Some(v) => {
                    assert!(finite, "cell {i} is {v} in JSON and absent in the plane");
                    let back = f16_to_f32(bits);
                    assert!(
                        (back as f64 - v).abs() < 0.07,
                        "cell {i}: {v} dB became {back} dB"
                    );
                    observed += 1;
                }
            }
        }
        assert!(
            observed > 0,
            "the fixture wrote no observed cell, so this comparison would pass on two empty grids"
        );

        // …and it is SMALLER, which is the whole point, measured on this very answer. The claim is
        // about the PLANE rather than the whole body, because at this test's 64-cell edge the
        // per-tile prose dominates; on a rendered 256-cell tile the plane IS the body (64 % of it,
        // the measurement this ticket started from).
        let as_text = plain["grid"]["max_db"].to_string().len();
        let as_plane = plane["data"].as_str().unwrap().len();
        assert_eq!(
            as_plane,
            (json_cells.len() * 2).div_ceil(3) * 4,
            "the packed plane's size must be a function of the CELL COUNT alone — two bytes a \
             cell, base64 — which is the property JSON decimal text does not have: the same grid \
             costs {as_text} B as text here and 1 197 118 B on the 256-cell live tile T-533 \
             measured, where the same cells packed are 174 764 B"
        );
        assert!(
            as_plane * 2 < as_text,
            "{as_plane} B packed vs {as_text} B as text"
        );
        assert!(
            packed.to_string().len() < plain.to_string().len(),
            "the packed body must not be larger than the JSON one"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unknown_plane_encoding_is_refused_rather_than_answered_in_another() {
        let dir = temp_dir("planes-bad");
        let (state, _, _) = state_with_records(&dir, N as i64, 0);
        let mut q = tile_params(F_INDEX, T_INDEX);
        q.push(("planes".into(), "f8".into()));
        let e = tiles_json(&state, &q).unwrap_err();
        assert_eq!(e.status, 400);
        assert!(e.message.contains("f8"), "{}", e.message);
        assert!(e.message.contains("json, f16"), "{}", e.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn decode_base64(s: &str) -> Vec<u8> {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let val = |c: u8| A.iter().position(|&a| a == c).expect("base64 alphabet") as u32;
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len() / 4 * 3);
        for c in b.chunks(4) {
            let pad = c.iter().filter(|&&x| x == b'=').count();
            let n = (val(c[0]) << 18)
                | (val(c[1]) << 12)
                | (if pad < 2 { val(c[2]) } else { 0 } << 6)
                | (if pad < 1 { val(c[3]) } else { 0 });
            out.push((n >> 16) as u8);
            if pad < 2 {
                out.push((n >> 8) as u8);
            }
            if pad < 1 {
                out.push(n as u8);
            }
        }
        out
    }

    fn f16_to_f32(b: u16) -> f32 {
        let s = if b >> 15 == 1 { -1.0f32 } else { 1.0 };
        let (e, m) = ((b >> 10) & 0x1f, (b & 0x3ff) as f32);
        match e {
            0 => s * m * 2f32.powi(-24),
            31 => f32::NAN,
            _ => s * (m + 1024.0) * 2f32.powi(e as i32 - 25),
        }
    }
}
