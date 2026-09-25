# 24 — The canvas as a data-projection surface

**Status: SPEC (T-800 / MAP-00, 2026-09-22) - the normative half is §13 (the layer registry), §14
(pins) and §15 (the client slices); §0-§12 are the argument behind them.** Proposed by the user on
2026-09-20, from a study of Google Maps and general mapping-software UX, and approved 2026-09-22
with the layout reference [`ui/mockups/map-ui-v1.html`](../ui/mockups/map-ui-v1.html). The decision
record is [ADR-0023](adr/0023-map-ui-and-research-state.md); the layout contract is
[`docs/23 §10-§11`](23-map-ui-philosophy.md). This document is an **extension of [`docs/16 §8`](16-coverage-tile-pyramid.md)
(MCANVAS — the unified full-spectrum canvas) and [`docs/14`](14-ui-rewrite.md) (the MUI rewrite)**. It
contradicts neither. MCANVAS already gives us the one WebGL2 surface, N scissored panes, the
minimap-as-viewport, `GET /api/tiles` at independent `(level_f, level_t)`, the four-state coverage
machine, the per-pane time-addressable spectrum trace, presence boxes drawn in the render pass, and
the retune-on-pan offer. What is **missing** — and what this document specifies — is the *chrome
reframe* (full-bleed, controls as overlays), the *layer model* that turns the waterfall into a surface
many data representations project into at once, first-class *pins*, the *Explore* drawer, and the
*research tooling* (durable collections, reusable measurements, annotations) that the user names as
exactly the thing Google Maps is bad at.

Everything here stays inside the two invariants that already bind the project: **the UI is a thin
client** — all signal logic lives in the backend behind [`docs/api.md`](api.md) and its contract tests,
and `ui/src` renders, maps pixels↔(Hz, time), and calls routes — and **the exploration-first rule** —
detection is blind first, and the known-signal database only ever *suggests*, ranked and reasoned,
never a source of truth (CLAUDE.md; ADR-0017/0019). This document is where those two invariants meet
the map idiom.

Companion document [`docs/23`](23-map-ui-philosophy.md) (the full-bleed chrome / immersive-shell
study) carries the symbology and figure-ground rules referenced in §8, and since T-800 the normative
layout contract in its §10-§11.

---

## 0. Why a spectrum canvas earns the full screen

A spectrum canvas earns the full screen the same way Google Maps does: not by hiding controls for
their own sake, but because **the content *is* the interaction**. You pan, zoom, hover and click the
data directly, so the data deserves every pixel and the controls become things that float over it.
Android's own guidance draws the line honestly — immersive chrome-removal is justified "only when the
benefit… goes beyond simply using extra screen space" ([Android: Immersive
content](https://developer.android.com/design/ui/mobile/guides/layout-and-content/immersive-content)) —
and a surface you pan, zoom and click qualifies, because navigation is the primary act.

Five principles, drawn from roughly fifteen years of convergent map-UI practice and the
cartographic/GIS-UX literature, reconciled with the MCANVAS invariants hackriff already holds:

1. **Full-bleed content, chrome as a z-axis overlay.** The surface occupies 100 vw × 100 vh. Controls
   do not live in a frame that shrinks the canvas; they float in semi-transparent panels docked to the
   viewport edges and can fade when idle (the W3C manifest display chain `fullscreen → standalone →
   minimal-ui → browser`; Fluent's *layering*; Material *surfaces*).
2. **Direct manipulation, with a hard line at the device.** Shneiderman's grammar — continuous
   representation of the object, physical gestures over commands, rapid/incremental/*reversible*
   operations with immediately visible effect ([Shneiderman
   1997](https://www.cs.umd.edu/~ben/papers/Shneiderman1997Direct.pdf)) — is exactly what a waterfall
   pan/zoom is. The crucial discipline, which hackriff already encodes and this design must never
   soften, is the line between manipulating the **representation** (reversible, local, zero external
   effect) and triggering a **device action** (explicit, discrete, separately confirmed). Google Maps
   separates "look around" from "get directions"; hackriff separates "a pan/wheel never commands the
   radio" from "selecting an un-tuned region retunes it". Full-bleed does not blur that line; it makes
   the line the whole safety model.
3. **The map is a projection surface, not an image with an overlay.** Google's documented Maps
   architecture is a base map plus stacked, independently-toggleable data **layers** sharing one
   coordinate system, with a dedicated `Data` layer for arbitrary geodata. Mapbox GL / MapLibre
   formalise it as **sources** (raw data) and **layers** (a rendering of a source), composited in
   explicit paint order on one GPU context; deck.gl and kepler.gl generalise each layer to a *pure
   function of (data, accessor)* over one view. hackriff's coordinate system is (time × frequency)
   instead of (lat, lng), and its base map is the coverage-tile waterfall — but the pattern is
   identical and already half-built.
4. **Progressive disclosure and semantic zoom.** Show what matters most first; defer the rest to
   on-demand affordances (Nielsen). Zoom changes *what* is shown, not just scale — and, the
   consistency constraint from the semantic-zoom literature, nothing introduced at a coarse level may
   vanish and reappear as you go deeper. This is precisely hackriff's three honesty tiers (live-IQ
   detail / spectrum-history / survey-overview): each zoom discloses only the detail the hardware
   actually justifies, never upscaled measurement dressed as fresh data.
5. **Figure-ground and honest symbology.** Cartography's first law: not all information is equal. The
   base recedes (low-contrast, desaturated) so the thematic overlay reads as the figure. For hackriff
   the raw spectrum energy is the ground; detections, pins, annotations and coverage-grey are the
   figure and must dominate even though they sit on denser data. Symbols use a small controlled
   vocabulary, stay legible small, collision-avoid their labels, and — because ~8 % of men have
   red-green colour-vision deficiency — never encode state in hue alone.

**The synthesis:** keep everything Google Maps got right (full-bleed, direct manipulation, layers,
pins, bottom sheets, progressive disclosure) and deliberately fix the two things the user names it
*bad* at — you cannot build a durable collection of your own marks, and you cannot reuse a
measurement. Those are not chrome problems; they are a **state-model problem**. The fix (from Felt,
kepler.gl, ArcGIS, IQEngine, Raven Pro) is to separate *ephemeral* exploration state (hover, transient
filter, current viewport) from *durable* research state (named collections, saved measurements with
units and provenance, annotations that are real exportable data rows), give the durable state its own
persistent surface (a table that is two views of one data with the canvas), and make export a
first-class path. A spectrum canvas that is a research instrument is a map that remembers what you
found.

---

## 1. Sources versus layers

Adopt the Mapbox/MapLibre/deck.gl distinction wholesale, because it is the vocabulary that lets many
data domains share one surface without entangling them:

- A **source** is raw data with a coordinate footprint: the tile pyramid, the event/inventory ledger,
  the coverage map, band-plan priors. A source knows nothing about how it is drawn.
- A **layer** is *one rendering* of a source with its own visual encoding (a fill, a stroke, a marker
  glyph, a hatch), composited in an explicit **paint order** on the one WebGL2 context.

The design commitment that makes this pay off: **each layer is a pure function of (data, accessor)**
([deck.gl](https://deck.gl/docs); [kepler.gl](https://docs.kepler.gl/)). A layer reads a source
through an accessor that maps a record to screen geometry via the pane's own capture-time and Hz
mappings, and emits primitives. It never mutates the source, never reaches another layer, and never
reads global state. The consequence hackriff needs: **a new data domain adds a layer without touching
any other** — the same reason Google Maps can add "Air Quality" or "Wildfires" over the same lat/lng
base without disturbing transit or terrain.

Mapping onto the renderer MCANVAS already built (docs/16 §8.3, §8.5a):

- **One context, scissored panes.** Layers draw into a pane's scissor rectangle in paint order. A pane
  carries its own centre/span in both axes; a layer is invoked once per pane with that pane's mapping.
- **Shared tile-texture LRU.** The base-waterfall layer uploads a tile once even when it is visible in
  two panes (T-437 proved 95 keys → 95 uploads, 8 panes sharing one tile). Overlay layers add no
  texture pressure of their own beyond their (small) geometry buffers; the LRU is unchanged.
- **Data-driven styling by zoom.** As in Mapbox's `step`/`interpolate` expressions, a layer's encoding
  is keyed off the pane's current `(level_f, level_t)` and honesty tier, so density and glyph size
  change with zoom without re-fetching — the mechanism §6's clustering and §3's honesty-tier shading
  both ride on.

A **layer registry** is per-pane (a pane is *where you look from* — docs/16 §8.2), holding an ordered
list of `{layer_id, source, accessor, visible, z}`. It is pure client-side presentation state: which
renderings of already-served data a pane composites, in what order. It commands nothing and stores no
signal logic.

---

## 2. One event, three renderings

The core move of a mapping UI is that a single result appears **in three places at once, off one
shared state**: as geometry drawn *on* the surface, as a row in a panel/table, and as a text summary.
Directions are drawn on the map *and* listed in a sheet *and* summarised as an ETA — never fetched or
computed three times, never allowed to disagree.

For hackriff, a detection/event is:

- **Geometry on the canvas** — a time-frequency box (or, sub-pixel, a marker/count — §6), drawn in the
  render pass through the pane's mapping.
- **A row in a panel/table** — the Candidate/Confirmed lists (docs/14), and the durable research table
  (§7).
- **A text summary** — the MapTip on hover and the detail sheet on select (§5), the big-frequency
  readout and ranked explanations.

**The sync rule:** the table and the map are *two views of the same data*. There is exactly one client
state object per event, sourced from the backend (`GET /api/events` / `/api/inventory`), and both the
geometry accessor and the row renderer read it. Selecting a row highlights the box; selecting the box
highlights the row; a liveness change (an interval reopening — ADR-0017) moves both. There is **never
a second source of truth** — no separately-maintained "boxes list" that a data poll advances while the
table lags, which was the class of the T-388 drift/jump bug. The backend already guarantees the two
*server* surfaces cannot disagree: `/api/events`, `/api/inventory`, `/api/inventory/{id}/presence` and
`/api/tiles/events` all derive presence through the one call `ObservedCoverage::track` (T-591,
[docs/api.md](api.md)). The client's obligation is to preserve that single-source discipline through
to the pixels.

---

## 3. The layer taxonomy for RF

Each layer below is one rendering of an *already-served* source, with a defined figure-ground role.
Paint order is bottom-to-top as listed; §4 makes visibility and order user-controllable per pane.

**(a) Base waterfall.** The spectrum energy itself — the ramp/phosphor render (T-475's amplitude
colour, smooth trace, afterglow), tiles from `GET /api/tiles` and the per-pane trace from the live-IQ
FFT. **Figure-ground role: the ground.** It is the densest data on screen and must recede — this is
the cartographic base map. It is desaturated relative to the overlays so detections read as figure
(§8).

**(b) Coverage-fog.** The four-state coverage plane from `GET /api/coverage` and the tiles' own
observed bitmaps, made an explicit, toggleable figure-ground layer. The **four states never collapse**
([docs/api.md](api.md), T-441/T-423/T-595): `unobserved` (nothing ever looked — **grey, and *only*
this is grey**), `observed`, `unknown` (we no longer know whether we looked — not grey, not a level),
and `excluded` (sampled, and deliberately left out of analysis — the receiver's own DC/LO notch; the
measurement exists and must be drawn). The live edge is a fifth, transient mark — *observed but not
yet measured* — "we are looking right now", neither grey nor `unknown` (docs/api.md, "No cell for the
newest moment"). **Grey = genuinely unobserved is the point**: across 6 GHz most of the canvas is
honestly empty, and the shape of what is *not* grey is the survey (CLAUDE.md). This layer is where
that shape becomes a controllable figure.

**(c) Detections / events.** Confirmed and Candidate boxes from `GET /api/events`, drawn as
time-frequency regions per the box invariant. **Confirmed is prominent; Candidate is weaker** (lower
contrast, thinner stroke, a distinguishing glyph — §8). Boxes are drawn *start → live-edge* by
assumption for an ongoing signal and capped only by a detected end (ADR-0017); the client advances the
box top on the per-frame render, never on a data poll (the T-388 rule). **Boxes are never stacked:**
real emissions essentially never overlap in time-frequency, so overlapping boxes are proof the
analysis is wrong. The backend detects the overlap and re-analyses the region to resolve it (the
overlap-is-an-error invariant); the client's job is to render what the backend resolved, and to *not*
paper over stacked boxes as if they were legitimate. Candidates churn — created and deleted
continuously — and the layer must handle that as normal, decaying/expiring a candidate's box when its
confidence does (ADR-0017), not accumulating ghosts.

**(d) Artifacts.** Image / harmonic / intermod / retune-sibling relationships from the inventory
`relation` field (`artifact-of`, `retune-sibling-of` — [docs/api.md](api.md) T-219/T-598, `hk_model::relate`),
drawn as **linked overlays** that visually tie a spur/image to its source (a connector stroke from the
artifact box to the source box, labelled with the mechanism: `image`, `n`th `harmonic`, `intermod`).
This serves the user's signal-relationships goal (MEMORY: *detect a signal is an image/IMD or
reflection of another*). It is honest by construction: the relationship is **ranked evidence with its
reasoning disclosed, never truth** (the `relation` object carries the arithmetic — `predicted_hz`,
`error_hz`, `suppression_db`, the receive chain it rests on), and a client presents the link as a
suggestion, not a merge. The layer reads it and draws it; it computes no relationship itself (thin
client).

**(e) Honesty-tier shading.** The three tiers — **live-IQ detail / spectrum-history /
survey-overview** — kept visually distinct, per pane, so a wide or deep zoom reads as overview rather
than upscaled detail presented as measurement. This is semantic-zoom consistency rendered as a layer:
each pane already states the level it was drawn at (`paneStatuses()`, docs/16 §8.5a); this layer makes
that statement a visible band/badge rather than only text. The tier a pane is drawn at comes from
`GET /api/navigation` / the tile `resolution.source` field — the backend decides the tier; the layer
draws the decision.

**(f) Band-plan priors.** A **suggestions** layer: ranked band-plan/licence allocations drawn *beside*
measurements as candidate explanations — "FM broadcast allocation nearby; 150 kHz off raster". This is
the exploration-first rule made visible: the database is **never a source of truth, never
pre-populates the inventory, never overrides what was measured**, and a mismatch (an FM station off
its expected frequency) is *flagged, not snapped* (CLAUDE.md; MEMORY). It needs one small new
read-only route (§7 of the spec / this document's §7), because the client cannot synthesise priors
(thin client) and a prior is per-caller and mutable, so it is not a tile channel. **Default off is
wrong here in one specific sense** (§4): the priors layer may default off, but *unknown signals must
never be hidden by default* — the priors layer is additive suggestion, never a filter that removes the
un-explained.

---

## 4. The layers menu — two independent axes

Follow Google Maps' own structure: the layers control exposes **two orthogonal axes**, toggled from
one lightweight affordance (a single "layers" button that opens a small floating panel, not a settings
page):

- **Base style** — the waterfall look: ramp vs phosphor (T-475), colour map, afterglow. One choice at
  a time, like satellite-vs-default. This is pure presentation over the base-waterfall layer.
- **Overlay content** — the independently-toggleable overlay layers (b)–(f) above, each with its own
  visibility and a defined z-order. Several may be on at once (unlike base style).

**Per-pane.** The menu acts on the focused pane's layer registry (§1), because a pane is an
independent viewpoint. A sensible default set is shared when a pane is created and then diverges as the
user toggles.

**Z-order and default visibility.** Paint order is (a) base → (b) coverage-fog → (c) detections → (d)
artifacts → (e) honesty-tier → (f) priors, i.e. suggestions on top so they never obscure a
measurement. Defaults: base waterfall, coverage-fog and detections **on**; artifacts, honesty-tier
badges and priors **off** until asked. **The hard rule on defaults:** a view that highlights only what
it has *explained* hides the interesting part. Unknown/un-explained detections are the priority to
surface (CLAUDE.md), so the detections layer must never default to showing only Confirmed-with-a-family
— Candidates and unknowns are visible by default, and any "explained-only" filter is an explicit,
reversible opt-in, clearly marked as hiding data.

---

## 5. Pins as first-class objects

Signal/event markers and human-placed marks become first-class interactive objects with **three
states**, following the converged map-marker pattern
([mapuipatterns.com](https://mapuipatterns.com/marker/); [Google advanced
markers](https://developers.google.com/maps/documentation/javascript/advanced-markers/accessible-markers)):

1. **Rest** — a glyph at its place, in the controlled vocabulary (§8): shape/hue/size encode
   candidate/confirmed/unknown and a coarse family, sized legibly (~11 px minimum, Mapbox's floor).
2. **Hover — a MapTip.** A lightweight readout without committing to navigation: centre / bandwidth /
   family-suggestion / on-air, drawn near the cursor and dismissed on leave. It reads already-loaded
   state; it fetches nothing and changes nothing.
3. **Selected — the detail sheet.** Opens the bottom sheet / slide-in (peek → half → full) with the
   full selected signal: big frequency, measurements *with their measured-at time* (the `measured`
   object, docs/api.md), ranked explanations (suggestions, never truth), and actions — Listen, Decode/
   RDS, Record clip, Stream out, Promote/Delete, Analyze (`POST /api/analyze`). This rehomes MUI's
   focus panel into the map idiom (docs/14); the sheet coexists with a still-live, still-pannable
   canvas rather than a fixed panel that eats width.

**Placement is in capture-time/frequency**, and pins **move with pan/zoom like every overlay** — they
re-lay-out on every render frame through the pane's own capture-time mapping, never at fixed screen
coordinates (the MCANVAS one-shared-time-axis invariant; the "boxes drifting out of step with the rows
is an invariant violation" rule).

**Accessibility.** Pins are independently keyboard-focusable (tab order over the visible set, arrow-key
nudge between neighbours), and hover/click behaviours have keyboard equivalents (focus = MapTip, enter
= select), per Google's own marker-accessibility guidance. State is never hue-only (§8).

**Two kinds of pin, kept distinct.** *Automatic detection markers* are drawn from `GET /api/events` /
`/api/inventory` and are ephemeral to the viewed window (they come and go as the inventory churns).
*Human-curated collection markers* are durable research objects (§7) the user placed deliberately.
They must be visually distinguishable — the OpenWebRX precedent colour-codes bookmarks by provenance
(green = band-plan-derived, yellow = shared, blue = personal) exactly so a glance tells you the trust
tier ([OpenWebRX bookmarks](https://github.com/jketterl/openwebrx/wiki/How-the-bookmarks-work)), which
maps cleanly onto hackriff's *measured-vs-suggested-vs-mine* distinction and its "never a source of
truth" rule. A curated marker is a first-class, exportable object; a detection marker is a rendering of
the live catalogue.

---

## 6. Clustering and the coarse-zoom honesty rule

Raw marker count must never be rendered 1:1 at coarse zoom. Pins **cluster** from the existing `GET
/api/tiles/events` aggregate (count-per-cell on exactly the tile's axes — [docs/api.md](api.md),
T-438), and clusters **resolve into individual pins on zoom-in**, mirroring the tile pyramid's own
coarsen/refine. This is the same coarsen/refine the base waterfall already does, applied to the point
layer — one consistent behaviour across the surface.

**The honesty rule, which the aggregate already enforces server-side and the client must not
violate:** a sub-pixel burst renders as a **minimum-size marker** or a **count**, *never* an inflated
box that fabricates a timespan. `/api/tiles/events` counts one event per cell at the cell holding its
**start**, refuses to inflate a long emission by counting it per crossed row, and refuses to scale a
sub-cell burst up to be visible ("clamping it to an edge would put activity in a cell it never
occupied"). The client honours the same discipline in reverse: at a zoom where a box would be smaller
than a marker, it draws the marker/count, not a fattened box. This is the point-layer form of the same
rule that governs the tiers (§3e) — never present more precision than the data holds.

**Picking is a first-class pipeline concern.** For dense point sets, hover/click hit-testing is not a
per-element DOM test but part of the render pipeline — deck.gl does GPU picking as part of every layer,
and hackriff's WebGL2 surface should do likewise (a picking pass that reads back the marker id under
the cursor), so hover/select stays responsive over thousands of markers. Clusters are pickable too:
hovering a cluster shows its count and band; clicking a cluster either zooms to resolve it or opens a
list of its members in the sheet.

---

## 7. The one backend addition — band-plan priors over a viewport

Every layer above reads a route that already exists **except** band-plan priors (§3f). Specify one
small **read-only** route:

```
GET /api/priors?f_lo&f_hi&t0&t1   (Hz, Hz, Unix s, Unix s)
  → ranked band-plan / licence allocations that intersect the viewport,
    each as an EXPLANATION: { f_lo_hz, f_hi_hz, service, allocation, source,
                              rank, reason, off_raster_hz? }
```

- **Gated identically to `/api/events`** — bearer token, same region parameters, same error shape,
  never audited (GET). Result size capped like the other query routes.
- **Ranked explanations, never truth.** The response is a *suggester*: allocations that plausibly
  explain measured energy in the viewport, ranked, each with a backend-rendered `reason` and, where
  relevant, how far off the raster a nearby measured emission sits — the "150 kHz off raster" flag.
  The route **never pre-populates the inventory**, **never sets a family**, and is drawn only in the
  priors layer, on top of measurements, off by default.
- **Why it is *not* a tile channel.** Priors are **mutable** (band-plan/licence reference data updates;
  offsets are computed against the *current* viewport's measured energy) and **per-caller** (the ranking
  depends on what this viewport actually measured). Sealing them into an immutable tile would spend the
  immutability the tile storage was bought for — the exact argument `/api/tiles/events` makes for not
  being a tile channel. So priors are computed **on demand** over the requested region, like
  `/api/analysis/strongest`.
- **Why the client cannot synthesise it.** The thin-client rule: all signal logic (including matching
  measured energy against band-plan reference data and ranking the explanations) lives in the backend.
  The database is a *suggester*, and the suggester is a backend capability behind `docs/api.md`, not a
  reference table shipped into `ui/src`. Shipping band plans to the client would also invite the client
  to *start* from the database — the precise thing exploration-first forbids.

Adding this route follows the standing rule (CLAUDE.md, T-079): update `docs/api.md` and its contract
tests (`crates/hk-cli/tests/api_contract.rs`) together, and add a `ui/test` assertion on the *request
the client builds* (the guard against T-367's wrong-request class of bug).

---

## 8. Symbology, figure-ground, accessibility

Cross-reference [`docs/23 §7`](23-map-ui-philosophy.md) for the full symbol table and
[`docs/23 §10.5`](23-map-ui-philosophy.md) for the accessibility floors; the load-bearing rules here:

- **Figure-ground.** The base waterfall recedes — low contrast, desaturated — so detections, pins,
  annotations and coverage-grey read as the figure ([Esri: figure-ground
  organization](https://www.esri.com/arcgis-blog/products/product/mapping/graphic-design-principles-for-mapping-figure-ground-organization)).
  The raw spectrum energy is the densest thing on screen and must not compete with the marks drawn on
  it.
- **A small controlled vocabulary.** Markers and boxes use shape + hue + size from a fixed set, not
  free-form styling: shape/pattern encodes *state* (candidate / confirmed / unknown, and
  observed / unobserved / unknown / excluded), hue a coarse category, size a magnitude (bandwidth or
  confidence). Legible at ~11 px.
- **Collision-aware labels.** Label placement collision-avoids and suppresses at density, so a busy
  band (902–928 MHz ISM — the canonical burst playground) degrades to markers and counts rather than
  a wall of overlapping text ([Mapbox: label
  placement](https://docs.mapbox.com/help/dive-deeper/optimize-map-label-placement/)).
- **Never hue alone.** ~8 % of men have red-green CVD, so candidate/confirmed/unknown and
  observed/unobserved each carry a **shape or pattern** in addition to colour, and red-green pairings
  are avoided ([Esri: designing for colourblind
  readability](https://www.esri.com/arcgis-blog/products/arcgis-pro/mapping/designing-maps-for-colorblind-readability)).
  This is why **grey stays reserved for genuinely-unobserved only** — grey is a state with a specific
  meaning, not a decorative background, and the other three coverage states carry their own non-grey
  marks (docs/api.md, §3b).

---

## 9. What this must not break

This is an extension, and it holds the existing guards:

- **Overlay quads are strokes, not washes.** A layer may outline, connect, mark and hatch, but it must
  **not tint a measurement** — the base-waterfall pixels are the measured level and an overlay that
  washed colour over them would make the same energy read as a different strength. The guard is
  concrete: the base waterfall renders **byte-identical with overlays on and off** (the same class of
  guard as the coverage strip's `shade` normalisation, and the "cannot tint a measurement" rule from
  docs/16 §8.5d). Coverage-fog is the one deliberate exception, and it is drawn as a distinct honest
  state (grey / hatch / badge), never as a colour multiplier over energy.
- **One ramp module.** The amplitude→colour mapping stays a single module (the T-397 guard); base-style
  choices in §4 select within it, they do not fork a second ramp. A strip drawn on one scale beside a
  waterfall drawn on another is the exact failure the `shade` block was written to prevent.
- **Coverage grey stays honest.** No layer may manufacture "observed" out of "unobserved" (max-hold
  must not turn the max of nothing into a level — docs/api.md), and no layer draws grey for anything but
  genuinely-unobserved. The live-edge, `unknown` and `excluded` states keep their own distinct marks.
- **One gesture vocabulary.** Authoring gestures (drag-to-select a region to measure or annotate)
  coordinate with the open **T-458** binding (drag pans, so a selection gesture needs a modifier or a
  mode) and **T-456** (wheel zooms; a modifier gives one axis), so a single canvas keeps one gesture
  grammar. A pan or wheel still **never commands the radio** (the spy-client empty-call-list assertion);
  only an explicit region-select offers/commits a retune through the one gated `DeviceAction` path.
- **Thin client throughout.** All four durable research stores (§ below), band-plan priors (§7), and
  every layer's data live in the backend behind `docs/api.md` and its contract tests. `ui/src` renders,
  maps pixels↔(Hz, time), and calls routes — it holds no signal logic.

---

## 10. Research tooling — the part Google Maps is bad at, made first-class

The user's specific complaint: Google Maps resists becoming a workspace — every mark is ephemeral,
scoped to one session, disconnected from any narrative you build; you cannot durably collect your own
pins, and you cannot reuse a measurement (you write it down elsewhere). The tools that succeed as
research instruments — Felt, kepler.gl, ArcGIS, IQEngine, Raven Pro — all do the same thing: **separate
ephemeral exploration state from durable research state, and give the durable state its own persistent
surface synced with the map.**

**Ephemeral (client-only, per-viewer):** hover, the transient filter, the current viewport, the layer
toggles, an unsent draft. These are legitimate thin-client presentation state (browser storage at
most).

**Durable (backend, behind `docs/api.md`):** four object kinds, each carrying **provenance** — what
view/config/time produced it, generalising hackriff's provenance-per-detection to
provenance-per-annotation:

1. **Marker collections.** Generalise the existing frequency-only `/api/bookmarks` (docs/api.md) into
   **named, toggleable-as-a-layer collections of time-frequency markers**. A collection is the unit of
   organisation, sharing and export — not an undifferentiated pile of pins ([Felt: tour the
   interface](https://help.felt.com/getting-started/tour-the-interface)). A collection renders as an
   overlay layer (§3/§4), distinct from the automatic detection layer (§5).
2. **Saved measurements.** A measurement — Δf, Δt, bandwidth, duration, symbol-rate/period cursors (à la
   inspectrum/Raven Pro selection boxes) — is a **persistent object with value + unit + place + time**,
   drawn *on* the canvas and kept, not a tooltip you copy elsewhere. This is the direct fix for the
   user's "I write it down somewhere else" complaint ([ArcGIS measure
   widget](https://pro.arcgis.com/en/pro-app/latest/help/mapping/navigation/measure.htm)).
3. **Annotations.** Durable, **SigMF-compatible**, clickable-to-navigate time-frequency notes — the
   IQEngine model, structurally the same as the Confirmed/History catalogue
   ([IQEngine](https://github.com/IQEngine/IQEngine)). An annotation is a real, portable, exportable
   data row, not decoration.
4. **Saved views.** Named, restorable (time × frequency) windows (ArcGIS bookmarks), shareable and
   exportable — a saved view is just a named point in the pane's view-arithmetic state.

**Every mark is also a row in a table.** Each durable object is simultaneously geometry on the canvas
*and* a row in an inspectable, sortable, **exportable** table, synced with the canvas as two views of
one data — the Felt model, and the §2 sync rule applied to research objects. **Export is a first-class
path out** (file/link, SigMF-adjacent), not an afterthought — the research artifact must outlive the
session.

**Thin-client discipline holds throughout:** all four stores live in the backend behind `docs/api.md`
and its contract tests (extending `/api/bookmarks`, `/api/selections`, and the annotation/measurement
stores); `ui/src` only renders, maps pixels↔(Hz, time), and calls the routes. Authoring gestures
coordinate with T-458/T-456 (§9) so the single canvas keeps one gesture vocabulary.

---

## 11. The chrome reframe, concretely

Today the Explore/Decode app-shell frames the canvas (sidebars, focus panel, outputs dock stacked
around it — docs/14). Reframe to Google-Maps geometry:

- **The surface is 100 vw × 100 vh.** Every control becomes a floating, semi-transparent overlay docked
  to a viewport edge, able to fade when idle. **Chrome floats in screen space; data (boxes, pins,
  cursor, axes) floats in content space** and re-lays-out every frame through the pane's capture-time
  mapping — the MCANVAS invariant, restated as the chrome/data split.
- **A floating top control cluster:** search / go-to-frequency, the layers button (§4), and a live /
  "my-location" FAB that snaps a pane back to the growing edge (the follow-live action).
- **Floating zoom affordances**, and **HUD axes** — floating frequency (bottom) and time (left) rulers
  with ticks + labels, anchored in content space and fading with the chrome. This finishes the open
  **T-459** ("a readout is not a ruler") inside the new layout.
- **The detail panel becomes a bottom sheet / slide-in** (peek → half → full) that coexists with a
  still-live, still-pannable canvas instead of permanently eating width ([Material: bottom
  sheets](https://m3.material.io/components/bottom-sheets/overview); [NN/g: bottom
  sheets](https://www.nngroup.com/articles/bottom-sheet/)).
- **An Explore drawer** — collapsed-by-default bottom sheet surfacing interesting places to go:
  quiet-but-active bands, recent anomalies, strongest current signals, and **past surveys** (jump to a
  prior coverage window). It composes from existing read routes — `GET /api/analysis/strongest`,
  `/api/events`, `/api/coverage`, `/api/history`, and the survey coverage from `GET /api/control/scan`
  — and coexists with the live surface, never modal over it. *(A dedicated POI-ranking route, if the
  composed form proves too heavy on the client, is a candidate small backend addition — speculative,
  not required for a first cut.)*

Pause still freezes the **view**, not the capture: a pane either follows the live edge or is frozen on
a window, and none of this chrome reaches a device route (the always-on capture/ring/detection
invariant).

---

## 12. Proposed tickets and time estimate

> **Superseded as a plan by [`docs/26`](26-map-ui-redesign-tickets.md)** (the approved MAP-00…MAP-25
> ticket set, in `docs/tasks.yaml` as T-800…T-825). Kept for the reasoning behind the phase order and
> the effort tiering.

*Effort tiers per `prompts/model-selection.md`; a `core_interface` tag marks work that touches the
schema, a stream/route contract, or the render pass and cannot go to Sonnet/Haiku alone. Estimates are
engineering days for one developer with the agent workflow, and are **planning-grade, not committed**.*

**Phase A — chrome reframe (no new data).**

| Ticket | Effort | Notes |
|---|---|---|
| CANVAS-CHROME-DESIGN — full-bleed layout + overlay/z-index model; ADR update to 0013 | Opus, `core_interface` | 1.5 d. Blocks the rest of Phase A. |
| Full-bleed shell: surface 100vw×100vh, chrome as edge-docked floating overlays, idle-fade | Sonnet | 2 d |
| Detail bottom-sheet (peek→half→full), rehoming MUI focus panel | Sonnet | 2 d |
| Floating top cluster (search / go-to-freq / follow-live FAB) + floating zoom | Sonnet | 1.5 d |
| HUD axes — content-anchored frequency + time rulers (closes **T-459**) | Opus | 2 d |
| Explore drawer over existing read routes | Sonnet | 2 d |

**Phase B — the layer model (renders already-served data).**

| Ticket | Effort | Notes |
|---|---|---|
| Per-pane layer registry + paint-order compositor over the one context | Opus, `core_interface` | 2.5 d. Blocks the layers below; the render-pass touch. |
| Layers menu — two axes (base style / overlay content), per-pane | Sonnet | 1.5 d |
| Coverage-fog layer (four states distinct, grey honest) | Opus | 2 d |
| Detections layer (confirmed/candidate, no-stack, per-frame box top) | Opus | 2 d. Overlap invariant. |
| Artifacts layer (linked image/harmonic/IMD overlays from `relation`) | Opus | 2 d |
| Honesty-tier shading layer | Sonnet | 1 d |
| Byte-identical-with-overlays guard + one-ramp guard tests | Sonnet | 1 d |

**Phase C — pins.**

| Ticket | Effort | Notes |
|---|---|---|
| Pin object: rest / hover-MapTip / selected states, content-anchored | Opus | 2.5 d |
| Clustering from `/api/tiles/events`, resolve-on-zoom, sub-pixel→marker/count | Opus | 2 d. Honesty rule. |
| GPU picking pass for dense point sets | Opus, `core_interface` | 2 d. Render-pass touch. |
| Pin accessibility (keyboard focus/select, non-hue state) | Sonnet | 1 d |
| Automatic-vs-curated marker distinction | Sonnet | 0.5 d |

**Phase D — band-plan priors (the one backend addition).**

| Ticket | Effort | Notes |
|---|---|---|
| `GET /api/priors` route + `docs/api.md` + contract test | Opus, `core_interface` | 3 d. Backend suggester; on-demand, gated like `/api/events`. |
| Priors layer (ranked explanations, off-raster flag, off by default) | Sonnet | 1.5 d |
| `ui/test` request-shape assertion | Haiku | 0.5 d |

**Phase E — research tooling (durable state).**

| Ticket | Effort | Notes |
|---|---|---|
| RESEARCH-STORE-DESIGN — the four durable object kinds, provenance-per-annotation, export; ADR | Opus/Fable, `core_interface` | 2 d. Data-model + schema. Blocks Phase E. |
| Marker collections — generalise `/api/bookmarks` to named time-frequency collections + collection layer | Opus | 3 d |
| Saved measurements — value+unit+place+time objects, drawn + kept | Opus | 3 d |
| Annotations — durable, SigMF-compatible, clickable-to-navigate | Opus | 3 d |
| Saved views — named restorable (t×f) windows | Sonnet | 1.5 d |
| Research table — sortable/inspectable/exportable, synced with canvas | Opus | 3 d |
| Export path (file/link, SigMF-adjacent) as a first-class action | Sonnet | 2 d |
| Authoring gesture coordination with T-458/T-456 (one gesture vocabulary) | Opus | 1.5 d. Closes/absorbs **T-458**. |

**Rough totals:** Phase A ≈ 11 d, B ≈ 12 d, C ≈ 8 d, D ≈ 5 d, E ≈ 19 d. **Whole programme ≈ 55
engineering-days**, of which Phases A–C (≈ 31 d) deliver the visible Google-Maps reframe and the layer/
pin model, and Phases D–E (≈ 24 d) deliver the two things the user names Google Maps *bad* at. Phases
A and B can overlap after their design tickets land; D and E are independent of each other and of C.
These are **planning estimates for a design document, not commitments** — the coordinator finalises
IDs, dependencies and `parallel_groups` when the tickets enter `docs/tasks.yaml`.

---

## 13. The layer registry (normative)

*The contract MAP-06 builds and MAP-07…MAP-13 extend. Rationale: [ADR-0023](adr/0023-map-ui-and-research-state.md) §2.*

### 13.1 A layer declares its plane, and the plane is fixed

"Layer" has meant three different mechanisms in §1–§12. The spec separates them, because putting a
coverage wash in the stroke pass or a detection box in the DOM would quietly retire the honesty
guards MCANVAS was built around.

```ts
/** Which pass a layer renders in. Fixed at registration; never changes at runtime. */
type LayerPlane =
  | "data"      // the tile pass: value plane + cell-rule state plane. May paint a cell.
  | "overlay"   // the stroke pass: OverlayQuad edges/bars. CANNOT paint a cell (no sampler, no ramp).
  | "dom";      // band-1 focusable marks, laid out in the render frame.

type LayerId =
  | "base"        // data    - amplitude ramp / phosphor over the tile value plane (T-475/T-505)
  | "coverage"    // data    - the four coverage states + the shadow tier, via the one cell rule
  | "tier"        // data    - honesty-tier banding, per pane
  | "detections"  // overlay - Confirmed / Candidate boxes (MAP-08)
  | "artifacts"   // overlay - image / harmonic / IMD connectors to their source (MAP-11)
  | "priors"      // overlay - band-plan allocations drawn as explanations (MAP-12)
  | "rules"       // overlay - retention bound, IQ horizon, HUD ticks, time cursor
  | "research"    // overlay - saved measurements + annotations (MAP-20/22)
  | `collection:${string}`   // overlay - one durable marker collection, toggled as a layer (MAP-21)
  | "pins";       // dom     - detection markers, curated markers, clusters (MAP-09/10)

interface Layer {
  readonly id: LayerId;
  readonly plane: LayerPlane;
  readonly z: number;      // orders WITHIN the plane only; never across planes
  visible: boolean;
}

/** A pane's registry: presentation state only. It commands nothing and holds no signal logic. */
interface PaneLayers {
  readonly paneId: string;
  readonly base: "ramp" | "phosphor";   // the base-style axis: exactly one
  readonly layers: readonly Layer[];    // the overlay-content axis: any number
}
```

**Planes render in a fixed order — `data`, then `overlay`, then `dom` — and `z` never crosses a
plane.** No toggle, ordering or user preference can put a stroke beneath a measurement's colour, or a
DOM mark beneath a stroke. A layer that wanted to wash colour over energy would have to change its
`plane`, which it cannot.

### 13.2 An `overlay` layer is a pure function, and they all reach one hook

```ts
type OverlayLayerFn = (pane: PaneView, edgeNs: number) => readonly OverlayQuad[];
```

- It reads its source through an accessor over store state, places each record through **that pane's
  own** `Box`/`PaneRect` mapping (so a record sits at its capture-time/frequency coordinate), and
  returns strokes. It mutates nothing, reads no other layer, and touches no global.
- The visible layers, sorted by `z`, are **concatenated into the single existing `marks` hook** —
  `marks: (pane, edge) => OverlayQuad[]` in `ui/src/app/centre/surface.ts`, which `SurfacePreview`
  already takes. There remains exactly one place overlay geometry is produced and one pass that draws
  it, so **the byte-identical-with-overlays-off guard needs no change and loses no force** however
  many layers exist.
- MAP-06 therefore *refactors* that one call site into a registry; it does not add a second path.

### 13.3 A `data` layer is cell-rule/ramp state, not geometry

`coverage`, `tier` and `base` toggle uniforms on the **one** ramp module (T-397) and the **one** cell
rule (`ui/src/surface/cellrule.ts`, T-440/T-520). That is why the coverage fog may honestly draw
`unobserved` grey, `unknown`, `excluded` and the ADR-0020 shadow at all: the state plane is the only
thing permitted to paint a cell, and it is decided by one rule. **The layers menu's coverage switch
sets a cell-rule flag; it never adds a quad.** Forking a second ramp or a second cell rule is the
defect these guards exist to catch.

### 13.4 Paint order and defaults

Within `overlay`, ascending `z`: `rules` (10) → `detections` (20) → `paths` (25, T-897) → `research` (30) →
`collection:*` (40) → `artifacts` (50) → `priors` (60). Suggestions sit on top so they never obscure a
measurement; rules sit at the bottom so a box that crosses one is drawn over it (the existing
`ringQuads`-before-`markQuads` order, preserved).

| Layer | Default |
|---|---|
| `base`, `coverage`, `detections`, `paths`, `rules`, `pins` | **visible** |
| `collection:*` | the collection's own stored `visible` |
| `tier`, `artifacts`, `priors`, `research` | hidden until asked |

**The hard rule on defaults:** a view that highlights only what it has *explained* hides the
interesting part. Unknown and Candidate detections are **never hidden by default**, and any
"explained-only" filter is an explicit, reversible opt-in that says in words that it is hiding data.

### 13.5 Scope and the per-pane rule

The registry is **per pane** — a pane is where you look *from*, so two panes may legitimately show the
same window with different layers on. A new pane inherits the creating pane's registry **by value**
and diverges thereafter. The registry is serialised to `localStorage` per viewer and must render
correctly when storage is unavailable.

### 13.6 What a layer may never do

- Tint, wash or multiply over the value plane (only `data` may paint a cell; the overlay program has
  no sampler and no ramp, which is what makes this structural rather than a rule to remember).
- Draw grey for anything but genuinely-unobserved, or manufacture "observed" out of "unobserved".
- Lay out on the data-poll cadence. Every layer re-lays-out in the render frame, through the pane's
  own capture-time mapping (T-388).
- Stack competing Confirmed/Candidate boxes as though overlap were a feature. Overlap is an error
  signal the backend re-analyses (ADR-0019); the client draws what the backend resolved, and an
  overlap that reaches the screen is a backend bug to report, never something to z-order away.
- Fabricate a timespan (see §14.3).
- Call a device route. Toggling a layer is presentation; the spy-client call list stays empty.

---

## 14. Pins (normative)

*The contract MAP-09 and MAP-10 build to. Rationale: [ADR-0023](adr/0023-map-ui-and-research-state.md) §3.*

### 14.1 Pins are DOM, in band 1, laid out in the render frame

Pins live in the `#pins` container (docs/23 §10.1 band 1): `pointer-events: none` on the container,
`auto` on each pin, positioned from **content space** through the pane's own capture-time and Hz
mappings, **in the same pass as the canvas draw**. They are DOM because a mark must be
keyboard-focusable and carry an accessible name, which a GPU glyph cannot; they are affordable as DOM
because §14.4 caps them.

### 14.2 Three states, and a glyph vocabulary that survives colour-vision deficiency

| State | Behaviour |
|---|---|
| **rest** | a glyph at its (t, f) place |
| **hover / keyboard focus** | a **MapTip**: centre, bandwidth, family suggestion, on-air. Reads already-loaded state; **fetches nothing, changes nothing** |
| **selected** | opens the detail sheet (docs/23 §10.3) and selects the same object in the Research table |

| Kind | Shape | Hue role |
|---|---|---|
| Confirmed emitter | filled square | category |
| Candidate | open diamond | category |
| Unknown / unexplained | open circle with `?` | category |
| Human-curated marker | filled pennant | provenance tier |
| Cluster | rounded count chip | count |

**Never hue alone.** Shape carries state so ~8 % of men are not excluded; red–green pairings are
avoided. Glyphs are legible at **≥ 11 px** with a **≥ 24 px** hit area. Keyboard: pins are tab-stops
over the visible set, focus shows the MapTip, `Enter` selects, arrow keys step to neighbours.

**Two kinds stay distinguishable.** *Detection markers* are a rendering of the live catalogue
(`/api/events`, `/api/inventory`) and churn with it. *Curated markers* are durable research objects a
human placed (`/api/collections`). A glance must tell measured-vs-mine apart — the OpenWebRX
provenance-colour precedent, with a shape cue so it survives CVD.

### 14.3 Clustering, and the coarse-zoom honesty rule

Clusters come from the existing **`GET /api/tiles/events`** count-per-cell aggregate, on exactly the
tile's own axes, and resolve into individual pins on zoom-in — the point-layer form of the pyramid's
coarsen/refine.

**The rule, which the aggregate already enforces server-side and the client must not violate in
reverse:** at a zoom where a detection's box would be smaller than a glyph, draw **the glyph or the
count** — *never* a fattened box that invents duration. The server refuses to inflate a sub-cell burst
or to count a long emission once per crossed row; the client refuses to widen one back. Clusters are
pickable: hover shows count and band, click either zooms to resolve or lists members in the sheet.

### 14.4 The element budget, picking, and the deferred GPU pass

- **Hard cap: 400 pin elements per pane.** Above it the pane draws the next coarser cluster level.
  The cap is what makes DOM pins defensible and the per-frame cost predictable.
- **MAP-09/MAP-10 owe a measurement, not an assumption** (the T-453 discipline applied to the client):
  per-frame layout + hit-test cost at the cap, on the reference machine, recorded in the ticket.
- **Picking is a CPU quadtree** over the laid-out set, rebuilt in the frame that lays the pins out.
- **GPU picking is deferred with a stated trigger:** if layout + hit-test exceeds **2 ms per pane at
  the cap**, the picking pass becomes its own ticket, and its accessibility cost (a parallel focusable
  tree) is known in advance.

---

## 15. Client state: the four new slices (normative)

*The contract every MMAP client ticket composes into. Rationale:
[ADR-0023](adr/0023-map-ui-and-research-state.md) §6; the store model is
[ADR-0013](adr/0013-ui-architecture.md) §3 and `ui/src/app/store.ts`, unchanged.*

Four slices are added in their owning areas' `slice.ts` and composed into `AppState` exactly like the
existing eight, so no panel ticket edits `state.ts` beyond one import line.

```ts
// app/map/slice.ts — ephemeral chrome + the ONE shared selection
type Tool = "navigate" | "measure" | "annotate" | "pin";
type SheetState = "hidden" | "peek" | "half" | "full";
type Selected =
  | { kind: "emitter"; id: string }
  | { kind: "marker" | "measurement" | "annotation" | "view"; id: string }
  | null;

interface MapState {
  map: {
    chrome: { idle: boolean; gotoOpen: boolean; layersOpen: boolean; tool: Tool };
    sheet: { state: SheetState; tab: "explore" | "detail" };
    research: { open: boolean; tab: "all" | "marker" | "measurement" | "annotation" | "view";
                filterToWindow: boolean; sort: string };
    selection: Selected;            // canvas, sheet and table all read THIS
    pins: { hoveredId: string | null };
  };
}

// app/map/layers-slice.ts — per-pane registries (presentation only)
interface LayersState { layers: Readonly<Record<string, PaneLayers>> }

// app/research/slice.ts — a mirror of the four durable stores, as served
interface ResearchState {
  research: {
    collections: Collection[]; markers: Marker[];
    measurements: Measurement[]; annotations: Annotation[]; views: SavedView[];
    loaded: Record<"collections" | "markers" | "measurements" | "annotations" | "views", boolean>;
    error: string | null;
  };
}

// app/map/priors-slice.ts — the current viewport's suggestions
interface PriorsState { priors: { rows: Prior[]; forWindow: Box | null; loaded: boolean } }
```

**Four rules the slices encode.**

1. **One selection, three renderers.** Canvas, detail sheet and Research table read the *same*
   `map.selection` and the *same* `research.*` rows. Selecting a row selects the mark and vice versa;
   a liveness change moves both. There is never a second "boxes list" a poll advances while the table
   lags — that was the T-388 class.
2. **Edits go through the backend, not around it.** Dragging a saved measurement cursor or editing a
   note is a `PUT`; the slice updates **from the response**. No surface short-circuits the round trip:
   a measurement's value is the backend's to compute and the audit entry is the backend's to write.
3. **`research.*` is a mirror, not a source.** It holds rows exactly as served, with no derived signal
   measurement computed in the browser (the existing `state.ts` rule).
4. **Ephemeral and durable never share a slice.** `map.*`, `layers` and `priors` are ephemeral
   (`localStorage` at most); `research.*` mirrors durable backend state. Keeping them apart in the
   store is what keeps them apart on screen (docs/25 §1).

---

## Sources

Maps philosophy and interaction:
- Direct Manipulation (UX Tigers) — https://www.uxtigers.com/post/direct-manipulation
- Direct Manipulation: Definition (NN/g) — https://www.nngroup.com/articles/direct-manipulation/
- Shneiderman, Direct Manipulation for Comprehensible, Predictable and Controllable UIs (1997, PDF) — https://www.cs.umd.edu/~ben/papers/Shneiderman1997Direct.pdf
- Direct manipulation interface (Wikipedia) — https://en.wikipedia.org/wiki/Direct_manipulation_interface
- What is Progressive Disclosure? (IxDF) — https://ixdf.org/literature/topics/progressive-disclosure
- Progressive Disclosure (NN/g) — https://www.nngroup.com/videos/progressive-disclosure/
- Immersive content (Android Developers) — https://developer.android.com/design/ui/mobile/guides/layout-and-content/immersive-content
- Preparing for the display modes of tomorrow (Chrome for Developers) — https://developer.chrome.com/docs/capabilities/display-override
- Making Fullscreen Experiences (web.dev) — https://web.dev/articles/fullscreen
- display — Web app manifest (MDN) — https://developer.mozilla.org/en-US/docs/Web/Progressive_web_apps/Manifest/Reference/display
- Fluent Design System (Wikipedia) — https://en.wikipedia.org/wiki/Fluent_Design_System
- Google Maps directions/sheets redesign (9to5Google, Feb 2024) — https://9to5google.com/2024/02/07/google-maps-directions-search-redesign/
- Google Maps directions/sheets redesign rolling out (9to5Google, Jul 2024) — https://9to5google.com/2024/07/14/google-maps-android-redesign/

Layers / data-projection surface:
- Layers — Maps JavaScript API (Google) — https://developers.google.com/maps/documentation/javascript/layers
- Data Layer — Maps JavaScript API (Google) — https://developers.google.com/maps/documentation/javascript/datalayer
- Custom Overlays — Maps JavaScript API (Google) — https://developers.google.com/maps/documentation/javascript/customoverlays
- Map Types — Maps JavaScript API (Google) — https://developers.google.com/maps/documentation/javascript/maptypes
- Mapbox GL JS: Create and style clusters — https://docs.mapbox.com/mapbox-gl-js/example/cluster/
- Mapbox: add markers — https://docs.mapbox.com/help/getting-started/add-markers/
- deck.gl documentation — https://deck.gl/docs
- visgl/deck.gl (GitHub) — https://github.com/visgl/deck.gl
- kepler.gl documentation — https://docs.kepler.gl/
- keplergl/kepler.gl (GitHub) — https://github.com/keplergl/kepler.gl
- Google Maps Help: Use layers — https://support.google.com/maps/answer/3092439
- Get to know Google Maps layers (9to5Google) — https://9to5google.com/2023/06/14/google-maps-layers/

Markers, sheets, discovery:
- Marker — Map UI Patterns — https://mapuipatterns.com/marker/
- Google Maps: accessible markers — https://developers.google.com/maps/documentation/javascript/advanced-markers/accessible-markers
- Bottom sheets — Material Design 3 — https://m3.material.io/components/bottom-sheets/overview
- Bottom Sheets: Definition and UX Guidelines (NN/g) — https://www.nngroup.com/articles/bottom-sheet/
- Map UI Design (Eleken) — https://www.eleken.co/blog-posts/map-ui-design
- OpenWebRX wiki: How the bookmarks work — https://github.com/jketterl/openwebrx/wiki/How-the-bookmarks-work

Cartography / GIS-UX / accessibility:
- Principles of Map Design in Cartography (Esri) — https://www.esri.com/arcgis-blog/products/arcgis-pro/mapping/design-principles-for-cartography
- Figure-ground Organization (Esri) — https://www.esri.com/arcgis-blog/products/product/mapping/graphic-design-principles-for-mapping-figure-ground-organization
- Visual Hierarchy in Cartography (Map Library) — https://www.maplibrary.org/1201/visual-hierarchy-in-cartography-design/
- Guide to map design (Mapbox) — https://www.mapbox.com/insights/map-design-process
- Optimize map label placement (Mapbox) — https://docs.mapbox.com/help/dive-deeper/optimize-map-label-placement/
- Designing Maps for Colorblind Readability (Esri) — https://www.esri.com/arcgis-blog/products/arcgis-pro/mapping/designing-maps-for-colorblind-readability
- Semantic Zoom (Emergent Mind) — https://www.emergentmind.com/topics/semantic-zoom
- Controls — Maps JavaScript API (Google) — https://developers.google.com/maps/documentation/javascript/controls

Research tooling / SDR precedent:
- Tour the interface (Felt Help Center) — https://help.felt.com/getting-started/tour-the-interface
- Editing layers (Felt Help Center) — https://help.felt.com/layers/editing-layers
- kepler.gl user guides — https://docs.kepler.gl/docs/user-guides
- Measure — ArcGIS Pro Documentation — https://pro.arcgis.com/en/pro-app/latest/help/mapping/navigation/measure.htm
- IQEngine (GitHub) — https://github.com/IQEngine/IQEngine
- IQEngine on rtl-sdr.com — https://www.rtl-sdr.com/iqengine-a-web-based-toolkit-for-sharing-and-analyzing-rf-iq-recordings/
- Raven Pro — Cornell Lab of Ornithology — https://www.ravensoundsoftware.com/software/raven-pro/
- SPACE: SPectrogram Analysis and Cataloguing Environment (arXiv) — https://arxiv.org/pdf/2207.12454
- Maia SDR — waterfall rendering architecture — https://maia-sdr.org/about/
- SDRangel spectrum markers — https://github.com/f4exb/sdrangel/blob/master/sdrgui/gui/spectrummarkers.md

Internal (cross-references, not URLs): `docs/14` (MUI rewrite), `docs/16 §8` (MCANVAS), `docs/07`
(data model), `docs/api.md` (routes and the four coverage states), `docs/23 §7` (symbology,
*forthcoming*), ADR-0013 (UI architecture), ADR-0017/0019 (signal & inventory model), CLAUDE.md
(thin-client and exploration-first invariants).
