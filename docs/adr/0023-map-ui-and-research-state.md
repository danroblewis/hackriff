# ADR-0023 — The map UI: floating chrome, a per-pane layer registry over one context, pins, and four durable research stores

**Status:** PROVISIONAL. The *product* half is the user's — the Google-Maps grammar (2026-09-21, docs/23–26) and the approved mockup [`ui/mockups/map-ui-v1.html`](../../ui/mockups/map-ui-v1.html) (2026-09-22) — and is recorded, not proposed. What is proposed is the **mechanism**: which pass each layer renders in, where pins live, how authoring re-binds one gesture, the four store contracts, and the client slices. T-800 (MAP-00), milestone MMAP. Use cases AWARE-042, AWARE-053, RESEARCH-003.

**Touches:** root `CLAUDE.md` "The view" (unchanged — **§8 is the line-by-line check against the `docs/23 §9` list**); [`docs/16 §8`](../16-coverage-tile-pyramid.md) (MCANVAS — **extended**, §8.7); [`docs/14`](../14-ui-rewrite.md) (MUI — **extended**); [ADR-0013](0013-ui-architecture.md) (the store and slice model — extended with four slices, not changed); [ADR-0017](0017-time-extent-signal-model.md)/[ADR-0019](0019-presence-as-an-interval-with-endpoints.md) (the box model — unchanged); [ADR-0020](0020-last-known-shadow-tier.md) (the shadow tier — becomes a data-pass layer). Specs: [`docs/23 §10–§11`](../23-map-ui-philosophy.md), [`docs/24 §13–§15`](../24-canvas-as-data-surface.md), [`docs/25 §10`](../25-spectrum-research-workflow.md). Routes: [`docs/api.md`](../api.md) "Reserved: the map-UI research routes".

---

## Context

MCANVAS (docs/16 §8) landed the engine: one WebGL2 context, N scissored panes each with its own `(center_f, span_f)` and `(center_t, span_t)`, a shared tile-texture LRU, the coverage state machine, a separate overlay pass that **has no sampler and no ramp** so an overlay is structurally incapable of tinting a measurement, and per-pane time-addressable traces. What it did not land is the *instrument*: the canvas is still framed by the MUI app shell, overlays are a hard-coded list rather than a registry, there are no first-class markers, and nothing a researcher finds survives the session.

docs/23–25 argued the design; docs/26 sized it into 26 tickets; the user approved both and the mockup. Three things remain genuinely undecided, and a ticket cannot be written without them:

1. **Where each new thing renders.** "Layer" is used loosely across docs/24 for three different mechanisms — the data pass's cell rule (coverage fog, the shadow tier, the amplitude ramp), the overlay pass's strokes (boxes, connectors, rules), and focusable DOM (pins, labels). Left loose, the first layer ticket would put a coverage wash in the overlay pass or a detection box in the DOM, and the byte-identical guard and the T-388 cadence rule would both go.
2. **Which gesture authors.** docs/23 §3 says authoring reuses `Shift + drag` (T-458); docs/25 §9 says authoring is an explicit mode. Those are different products. One of them has to win, and the loser has to be shown to lose nothing.
3. **The store contracts.** Four durable object kinds, three of them with no code behind them, all four needing one provenance stamp and one paging discipline — and a `POST` shape that cannot be used to smuggle a client-computed measurement into the record.

Plus the two layout choices the mockup left open (sheet-at-every-width; Research as a slide-in).

---

## Options considered

**For rendering (decision 2).**
- *(a)* One flat layer list whose entries each say "draw me", each free to pick its own mechanism. Rejected: it is the current looseness with a registry wrapped round it, and it puts the honesty guards at the mercy of each layer author.
- *(b)* Everything through the overlay pass, including coverage. Rejected: coverage is a **state per cell**, not a stroke; drawing it as translucent quads over energy is exactly the wash the overlay pass was built to make impossible (docs/16 §8.5d), and the four coverage states would stop being decided by one rule.
- *(c) **chosen*** — a layer **declares its plane**: `data`, `overlay` or `dom`. Planes are ordered and fixed; `z` orders only *within* a plane. A layer cannot change plane at runtime, so no toggle can move a measurement's colour into a stroke's hands or vice versa.

**For pins (decision 3).**
- *(a)* GPU-picked point sprites in the overlay pass. Rejected **for now**: the overlay program has no sampler by design, so pin glyphs would need a second program and a picking read-back, and a GPU glyph cannot take keyboard focus or carry an accessible name — the accessibility requirement (docs/24 §5) would need a parallel invisible DOM tree anyway.
- *(b) **chosen*** — DOM pins in one content-anchored container, laid out **in the render frame**, with the element count bounded by clustering. GPU picking becomes a measured follow-up with a stated trigger, not an assumption (the T-453 discipline: measure the per-frame cost, don't assume it).

**For authoring (decision 4).**
- *(a)* `Shift + drag` authors. Rejected: `Shift + drag` already means *mark a region* (T-458), and a modifier that means "select" in one tool and "create an annotation" in another is precisely the second interpretation of a drag that T-412 cost us.
- *(b) **chosen*** — an explicit, visible **tool mode** re-binds the **bare** drag; `Shift + drag` keeps its one meaning in every mode. Authoring adds no gesture to the vocabulary; it changes what the default one does, visibly, reversibly, with `Esc` always returning to Navigate.

---

## Decision

### 1. The surface is the viewport, and the z-ladder has four bands

The canvas is `100vw × 100vh` (`position: fixed; inset: 0`). Everything else floats above it in one of four fixed bands, and **the band decides the coordinate system**:

| Band | z | Space | What lives here | Re-laid-out |
|---|---|---|---|---|
| 0 | 0 | content | the one `<canvas>`: tiles, traces, coverage, every stroke overlay | every frame |
| 1 | 10 | content | `#pins` — focusable marks anchored in (capture time, Hz) | **every frame, in the same pass as band 0** |
| 2 | 20 | screen | docked chrome: Go-to, layers, tools, zoom, follow-live FAB, pane status, HUD axis labels | on interaction |
| 3 | 30 | screen | sheets and the Research slide-in | on interaction |
| 4 | 40 | screen | transients: MapTip, retune offer, mode banner, errors | on interaction |

**No overlay is draggable or repositionable; users choose visibility only** (docs/23 §10.6 rule 3, amended 2026-09-24, T-896): each band-2/3 panel has the one dock docs/23 §10.2 assigns it, and no dock position is persisted — only visibility and the sheet's snap (size) state.

**Chrome floats in screen space; data floats in content space.** Band 1 is the only DOM allowed to be content-anchored, and it pays for that by being laid out inside the render frame — never on a data poll, never on a timer. A band-1 element positioned from a poll is the T-388 bug re-introduced, and is a defect, not a style.

HUD axis *rules* (ticks) are band 0 strokes; their *labels* are band 2 text positioned from the same per-frame mapping. Splitting them this way is deliberate: a tick is data geometry, a label is chrome that must remain selectable and legible at any DPR.

### 2. A layer declares its plane; the registry is per pane; the quads still come from one hook

```ts
type LayerPlane = "data" | "overlay" | "dom";
type LayerId =
  | "base"            // data   — amplitude ramp / phosphor, the tile value plane (T-475)
  | "coverage"        // data   — the four coverage states + shadow, via the one cell rule
  | "tier"            // data   — honesty-tier banding per pane
  | "detections"      // overlay— Confirmed / Candidate boxes
  | "paths"           // overlay— traced (t, f) routes: chirps, sweeps, hop sequences (T-897)
  | "artifacts"       // overlay— image / harmonic / IMD connectors
  | "priors"          // overlay— band-plan allocations, as explanations
  | "rules"           // overlay— retention bound, IQ horizon, HUD ticks, time cursor
  | "research"        // overlay— saved measurements + annotations
  | `collection:${string}` // overlay — one durable marker collection, toggled as a layer
  | "pins";           // dom    — detection + curated markers, clusters

interface Layer {
  readonly id: LayerId;
  readonly plane: LayerPlane;   // fixed at registration; never changes at runtime
  readonly z: number;           // orders WITHIN the plane only
  visible: boolean;
}
```

- **Planes render in a fixed order — `data`, then `overlay`, then `dom` — and `z` never crosses a plane.** No toggle, no ordering and no user preference can put a stroke under a measurement's colour or a DOM mark under a stroke.
- **`overlay` layers are pure `(pane, edge) => OverlayQuad[]`.** They read the store through an accessor, place records through *that pane's own* `Box`/`PaneRect` mapping, and return strokes. They mutate nothing, read no other layer, and touch no global. They are composed — filtered by `visible`, sorted by `z`, concatenated — into the **single existing `marks` hook** (`ui/src/app/centre/surface.ts`, the `marks: (pane, edge) => OverlayQuad[]` callback `SurfacePreview` already takes). There is therefore still exactly one place overlay geometry is produced and one pass that draws it, and **the byte-identical-with-overlays-off guard needs no change and keeps its full force**.
- **`data` layers are cell-rule/ramp state, not geometry.** They toggle uniforms on the existing single ramp module (T-397) and the single cell rule (`cellrule.ts`, T-440/T-520), which is why the coverage fog can honestly draw grey/`unknown`/`excluded`/shadow at all: it is the plane that owns state, and it remains the only thing that may paint a cell. The layers menu's "coverage fog" switch therefore sets a cell-rule flag; it does not add a quad.
- **`paths` (T-897, docs/23 §10.6 rule 2)** is an ordinary `overlay` layer at `z` 25, between the boxes it belongs to and the user's research marks. Its records come from `GET /api/paths` — ordered `(t, f)` vertices at absolute capture time, derived server-side from stored detections (`hk_model::path`) — and are laid out per frame through the pane's own box by `ui/src/surface/paths.ts`. A sloped segment is drawn as a run of stroke-thick axis-aligned steps, so the overlay program gains no primitive and stays incapable of expressing a measurement colour; the byte-identical guard covers it unchanged.
- **The registry is per pane** (a pane is where you look *from*). A new pane inherits the creating pane's registry by value and diverges thereafter.
- **Default visible:** `base`, `coverage`, `detections`, `paths`, `rules`, `pins`, and every collection whose stored `visible` is true. **Default hidden:** `tier` badges, `artifacts`, `priors`, `research`. Unknown and Candidate detections are **never** hidden by default, and any "explained-only" filter is an explicit, reversible opt-in that says it is hiding data.

### 3. Pins are DOM, bounded by clustering, and never fabricate a timespan

- **Three states:** rest (glyph), hover/focus (MapTip — reads loaded state, fetches nothing), selected (opens the detail sheet). Keyboard reaches all three: pins are tab-stops in the visible set, focus shows the MapTip, `Enter` selects.
- **State is never hue alone.** Confirmed = filled square, Candidate = open diamond, unknown = open circle with `?`, human-curated = filled pennant, cluster = a count chip. Minimum glyph 11 px (Mapbox's legibility floor), minimum hit target 24 px.
- **Clustering comes from `GET /api/tiles/events`,** the count-per-cell aggregate that already refuses to inflate a sub-cell burst or to count a long emission once per crossed row. The client honours the same rule in reverse: **below the size at which a box would be smaller than a glyph, draw the glyph or the count — never a fattened box that invents duration.**
- **The element budget is a hard cap: 400 pin elements per pane.** Above it the pane draws the next coarser cluster level. The cap is what makes DOM pins defensible; it is also what makes the cost predictable, and MAP-09/MAP-10 must *measure* per-frame layout cost against it rather than assume it (T-453's rule, applied to the client).
- **Picking is a CPU quadtree over the laid-out set**, rebuilt in the frame that lays the pins out. **GPU picking is deferred with a stated trigger:** if a pane's pin layout + hit-test exceeds 2 ms at the cap on the reference machine, the picking pass becomes its own ticket. Deferred, not assumed away.

### 4. One gesture vocabulary: a visible tool mode re-binds the **bare** drag; `Shift + drag` never changes meaning

| Gesture | Navigate (default) | Measure | Annotate | Pin |
|---|---|---|---|---|
| bare drag | pan both axes | lay measurement cursors | draw an annotation box | — |
| bare click | select mark / deselect | — | drop a text note | drop a marker |
| `Shift + drag` | **mark a region** | **mark a region** | **mark a region** | **mark a region** |
| wheel (+ modifiers) | zoom, per `ui/CONTROLS.md` | *unchanged* | *unchanged* | *unchanged* |
| `Esc` | — | → Navigate | → Navigate | → Navigate |

The active tool is always visible (a pressed button, a cursor change, and a mode banner naming what a drag will do). A region marked with `Shift + drag` is the input to *every* action that needs an extent — the retune offer (T-444), "measure this", "annotate this" — so **authoring is reachable without entering a mode at all**, which is why mode-switching costs the navigate-first user nothing. This resolves the docs/23 §3 ↔ docs/25 §9 conflict in favour of the mode, and closes T-445's open capability #2.

**None of it commands the radio.** Pan, wheel, pause, scrub, split, follow, every layer toggle, every sheet, every authoring action and every research write are view or durable-state acts: the spy-client call list stays empty. The one navigation act that may command the radio is unchanged — reaching un-tuned *frequency*, offered on a pan and committed by an explicit press through the one gated `DeviceAction` path.

### 5. Four durable stores, one provenance stamp, one paging contract, and a `POST` that cannot carry a value

Four backend stores behind `docs/api.md`, each audited, each `503 unavailable` without an audit log, none of them carrying a `device` key (authoring never reaches the radio):

| Store | Route family | Ticket |
|---|---|---|
| Marker collections (+ members) | `/api/collections`, `/api/markers` | MAP-17 |
| Saved measurements | `/api/measurements` | MAP-18 |
| Annotations | `/api/annotations` | MAP-16 |
| Saved views | `/api/views` | MAP-19 |

- **One provenance stamp, written by the backend** (docs/25 §2), never by the client: `device_id`, the tune in force, `t_capture` on the **capture clock**, `authored_s` on the **wall clock** — never conflated — `tier`, `actor` (a token fingerprint, never the token), and `authored: true`. The client sends the *view context it was on*; the server stamps what that means.
- **One paging contract for all four**, the `/api/events` one (`limit`, `cursor`, `count`/`matched`/`next_cursor`). "Durable" is not "unbounded": a list route that could grow without limit is a defect whichever store it belongs to. Defaults differ (annotations 200, the rest 500); every store accepts the same optional `f_lo`/`f_hi`/`t0`/`t1` window box, and `/api/annotations` **requires** it, because a research session accumulates annotations the way the catalogue accumulates events.
- **`POST /api/measurements` carries cursors, never a value.** A body containing `value` or `unit` is `400 invalid`. A bandwidth is a function of the noise floor and the −3 dB points, and a symbol rate of the signal — measurements over data, which by the thin-client rule are the backend's. The client's live drag readout is ephemeral presentation arithmetic over its own pixel↔(Hz, s) maps; *saving* is what crosses into an object, and the object's value is computed server-side. This is the thin-client rule made enforceable by the contract rather than by review.
- **Bookmarks are not broken and not duplicated.** The existing frequency-only `/api/bookmarks` becomes a **compatibility facade** over one reserved, un-deletable collection; existing bookmarks migrate in as frequency-only markers (`t_center_s = null`) keeping their ids and timestamps. One store, one facade — the alternative (two stores for one idea) is the drift the canvas cutover existed to kill.
- **A saved view is its own store, not a marker.** It is a serialised point in view-arithmetic state — `(center_f, span_f)`, an optional `(center_t, span_t)`, `follow_live`, an optional pane layout — and has no frequency *centre* in the sense a marker does. Restoring one is view arithmetic and moves no radio **unless** its frequency extent lies outside the tuned window, in which case it inherits the same gated retune offer a pan does. It gets no exemption.
- **Nothing here feeds blind detection.** An authored mark mints no candidate, moves no threshold, confirms no emitter, and never pre-populates the inventory — on create *or on import*. Detection's inputs are the air.

### 6. The client stays vanilla: four slices, no new dependency

Four new slices in the existing store (ADR-0013 §3), composed into `AppState` exactly like the others:

| Slice | Register | Holds |
|---|---|---|
| `map` | ephemeral | chrome idle/fade, open menus, active tool, sheet state + tab, **the one shared `selection`** |
| `layers` | ephemeral (localStorage) | per-pane registries: `visible`/`z` per `LayerId`, base style |
| `research` | durable mirror | the four stores' rows, as served; `loaded`/`error` per kind |
| `priors` | ephemeral | the current viewport's `GET /api/priors` answer |

**One selection, two renderers.** Canvas, sheet and Research table read the *same* `selection` and the *same* research rows; an edit is a `PUT` and both surfaces re-render from the response. No surface keeps a private copy, and no surface short-circuits the round trip — the measurement's value and the audit entry are the backend's to produce.

**No new frontend dependency is taken.** The justification is not taste: (i) the approved mockup implements every new surface — sheet with three snap states, layers menu, pins with clustering, MapTip, Research table, measure mode — in ~700 lines of vanilla TS/CSS with no library; (ii) the store already gives selector subscriptions with change-equality, which is the only reactive primitive the panels need; (iii) a framework's own scheduler would introduce a **second render cadence competing with the per-frame RAF pass**, and a poll-cadence layout against a frame-cadence scroll is the exact T-388 defect class this design is bound not to re-create; (iv) one developer, one operational surface (root `CLAUDE.md`: favour low operational complexity). A virtual-DOM diff also cannot be allowed near band 1, which must be positioned in the render frame.

### 7. The two layout choices the mockup left open

- **The bottom sheet is a bottom sheet at every width.** One component, one state machine (peek → half → full), one set of tests, and reachability on the handheld form factor the product is actually for. On wide screens it is width-capped (≈520 px) and docked to the bottom-left so it never covers the centre of the surface; it is never modal, and the canvas stays live and pannable beneath it.
- **Research is a right slide-in, not the sheet's full state.** The sheet is *selection-scoped and ephemeral*; Research is *durable and cross-window*. Merging them would erase the ephemeral/durable distinction docs/25 §1 requires to be visible, and would make the two impossible to use together — yet "click a row, watch the mark select and the detail sheet fill" is the whole point of *every mark is also a row*. Both may be open at once. At phone width the slide-in becomes a full-height panel and the sheet drops to peek.

---

### 8. The `docs/23 §9` checklist, line by line

*This is the acceptance list for MAP-00 and the list every later MMAP ticket is measured against. A
proposal failing any line is wrong, not a trade-off.*

| Invariant (`docs/23 §9`) | How this ADR holds it |
|---|---|
| **One surface over one (time × frequency) window** | Layers, pins, sheets and the Explore drawer are all views over the current selection. No second coordinate system, no second subject; the pane model, the minimap-as-viewport and the one-view-window-with-many-SDRs rule are untouched. A pane's layer registry changes what that pane *composites*, never what it is a view *of*. |
| **One shared absolute-time axis** | §1: the **band decides the coordinate system**. Bands 0 and 1 are content space and are laid out **every render frame** through the pane's own capture-time mapping; bands 2–4 are screen space and never carry data geometry. HUD ticks are band 0, labels band 2, both from the same mapping, so they cannot drift. §2 forbids any layer laying out on the poll cadence. |
| **Grey = genuinely unobserved, and it is the point** | §2: coverage, the tiers and the ADR-0020 shadow stay in the **`data` plane**, decided by the one cell rule; the layers menu's coverage switch sets a flag, it does not add a quad. Only `data` may paint a cell, so no overlay, base style or interpolation can paint over unobserved space. `observed-not-yet-measured`, `unknown` and `excluded` keep their own marks. |
| **A detection is a box `[start, end?]`; never fabricate a timespan** | §3 and docs/24 §14.3: clusters come from `/api/tiles/events`, and below glyph size the client draws **the glyph or the count**, never a fattened box. Boxes still run start → live edge until a detected end, advanced in the render pass. |
| **Overlap is an error signal** | §2 ("what a layer may never do", docs/24 §13.6): the detections layer never stacks or z-orders competing boxes as a feature. Overlap remains the backend's re-analysis trigger; an overlap reaching the screen is a backend bug to report. |
| **Navigation = view arithmetic in time + a device command in frequency** | §4: the whole new control vocabulary — pan, wheel, pinch, pause, scrub, split, follow, layer toggles, sheets, menus, tool modes, every authoring action, every research write — leaves the spy-client call list **empty** (MAP-25). The one exception is unchanged and is a discrete press through the gated `DeviceAction` path; §5 refuses saved views an exemption from it. |
| **Pause freezes the view, not the capture** | Untouched. A pane's pause *is* its time window; nothing in the chrome, the registry, the pins or the four stores reaches a device route or slows the SDR, the ring or detection. |
| **The three honesty tiers stay visually distinct; each pane states its level** | The `tier` layer makes the existing per-pane statement a visible band as well as text, and §1 forbids idle-fade from dimming any honesty statement. §5 records the `tier` an authored object was made at, so a mark taken off overview data says so forever. |
| **The client stays thin** | §5: all four stores and the priors suggester are backend capabilities behind `docs/api.md` and its contract tests; §5's cursors-never-a-value rule makes the line enforceable by the contract rather than by review. §6: `research.*` is a mirror of served rows with no derived measurement computed in the browser. Every panel owes a `ui/test` assertion on **the request it builds** (the T-367 guard). |
| **The view opens on the observed extent from the coverage map** | Untouched. `surface/bootstrap.ts` still decides first paint; no chrome, layer, pin or saved view may substitute a default span, a band-plan entry, `frequency.current` or a browser clock. A saved view is *restored on request*, never the bootstrap. |

---

## Consequences

- **The honesty guards get stronger, not weaker.** Because overlay layers are composed into the one existing `marks` hook and `data` is a separate plane no overlay can enter, the byte-identical-with-overlays-off test, the one-ramp test and the one-cell-rule test all keep their force over a surface with ten times as many layers. A layer that wanted to wash colour over energy would have to change its `plane`, which is fixed at registration.
- **Pins cost DOM, and the cap is what makes that safe.** 400 elements per pane laid out per frame is a real budget, and MAP-09/MAP-10 owe a measurement against it. If it does not hold, the fallback is specified (GPU picking + sprite glyphs) and the accessibility cost of that fallback is known in advance (a parallel focusable tree).
- **The tool mode is new UI state a user can get stuck in.** Mitigated by: the mode is always visibly named, `Esc` always exits, and every authoring action is *also* reachable from a `Shift + drag` region without a mode.
- **Four stores is four schemas and four migrations.** The paging contract, the provenance stamp and the error/audit shape are shared deliberately so the four tickets are four instances of one pattern rather than four designs; MAP-16 lands the pattern and MAP-17–19 follow it.
- **`/api/bookmarks` gains a second implementation path** (the facade). That is the cost of not breaking a live route; it is bounded by the facade being a *view* of one collection rather than a second store.
- **Priors are computed on demand and never sealed into a tile.** They are mutable and per-caller, so tile immutability — the thing the tile store was bought for — would be spent on them. The cost is a per-viewport query; the alternative was a lie about what a tile is.
- **What this does not do:** it does not re-open any ACCEPTED ADR, does not change the box model, the coverage states, the pane model, the tile pyramid, the gesture-to-device line or the capture path. No backend real-time code is touched by MMAP at all; the four stores sit beside the control API, not in the pipeline.

## Status

PROVISIONAL. The product direction and the mockup are the user's and are settled; the mechanisms above are this ADR's proposals and are cheapest-to-reverse where they could be wrong — DOM pins behind a cap with a stated GPU fallback, a tool mode that adds no gesture, four stores sharing one pattern so a change to the pattern is one change. It becomes ACCEPTED when MAP-06 (the registry), MAP-09/MAP-10 (the pin budget, measured) and MAP-16 (the first store, with its contract tests) have landed and held.
