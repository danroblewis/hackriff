# 23 — Map-UI design philosophy

**Status: SPEC (T-800 / MAP-00, 2026-09-22).** The normative half is §10 (the layout contract) and
§11 (the panel → state → route map); §1–§9 are the argument behind them, and **§9 is the invariant
checklist every MMAP line must hold**. Proposed by the user on 2026-09-21 as the design brief for
turning the MCANVAS surface into a full-bleed, Google-Maps-grammar research instrument, and approved
2026-09-22 together with the layout reference [`ui/mockups/map-ui-v1.html`](../ui/mockups/map-ui-v1.html)
— which stands to this design as `explorer-v3.html` did to MUI: the **layout and interaction
reference**, not a shipping artefact. The decision record is
[ADR-0023](adr/0023-map-ui-and-research-state.md). This document
**extends** [`docs/14`](14-ui-rewrite.md) (the MUI thin-client rewrite) and
[`docs/16 §8`](16-coverage-tile-pyramid.md) (the unified full-spectrum canvas). **It contradicts
none of their invariants** — §9 is the explicit checklist against them — and where it adds a control,
a layer or a durable object, that thing lives behind [`docs/api.md`](api.md) and its contract tests,
because the client stays thin.

This is the *why* and the *shape*. The applied design — the layer model, pins and the chrome
reframe — is [`docs/24`](24-canvas-as-data-surface.md); the durable-research data model (collections,
measurements, annotations) is [`docs/25`](25-spectrum-research-workflow.md); and the *what to build
and in what order* is [`docs/26`](26-map-ui-redesign-tickets.md) (the ticket set and estimate). These
are companion documents in the same set.

---

## 1. Why a map, and why now

The user's framing is exact and worth quoting rather than paraphrasing:

> In Google Maps, the map itself is a platform for rendering data onto. It's not just an overlay,
> it's a surface to project other data into. […] Now, Google Maps is not the best for research, and
> I have always faulted it for that. It's very difficult to create your own collection of pins. It's
> not easy to use the measurements for anything. […] But I think the Google Maps style UI would be a
> good basis for it.

Two asks are folded together here, and the design has to serve both without letting either soften the
other:

1. **Adopt the Google-Maps UI *grammar* for exploration.** Full-screen content, chrome floating on
   top, pins you hover and click, layers you toggle, an Explore drawer, a bottom sheet for detail.
   Fifteen years of convergent map-UI practice have made this grammar standard enough to name its
   parts directly, and hackriff has already half-built the hardest part of it (one WebGL2 surface
   over a coverage tile pyramid).
2. **Fix the two things Google Maps is *bad* at.** You cannot build a durable collection of your own
   marks, and you cannot reuse a measurement — you write it down somewhere else. These are not chrome
   problems; they are a **state-model** problem, and they are exactly the capabilities a research
   instrument lives or dies by. A spectrum canvas that is a research tool is *a map that remembers
   what you found.*

"Why now" is that MCANVAS (`docs/16 §8`, landed 2026-09-17) already delivered the surface these ideas
need: one WebGL2 context, N scissored panes each with its own `(center_f, span_f, center_t,
span_t)`, the minimap-as-viewport (since retired by the user, 2026-09-25, T-995: the whole range is reached by zooming out, and its per-SDR capture segments are drawn in the panes), a shared tile-texture LRU keyed by `(level_f, level_t, f_block,
t_block)`, the coverage state machine, and per-pane time-addressable spectrum traces (T-457/T-475).
What is missing is not the engine — it is the **chrome reframe**, the explicit **layer model**,
first-class **pins**, the **Explore drawer**, and the **research tooling**. This document is the
design contract for those, bound so that no later ticket can drift back into a framed, ephemeral UI.

The rest of the document draws on five research lenses — general maps philosophy, the map-as-data-
surface architecture, professional cartographic/GIS UX, research-grade tooling (Felt, kepler.gl,
ArcGIS, IQEngine), and the SDR-waterfall prior art (Maia SDR, SDRangel, OpenWebRX, Raven Pro) — cited
inline and listed in [§ Sources](#sources).

---

## 2. Chrome as overhead; content as the whole canvas

**"Chrome"** is the settled word for the window and application furniture that surrounds content:
address bars, navigation buttons, frames, toolbars, docked panels. The web platform even encodes a
fallback *chain* of how much of it to strip, in the web-app-manifest `display` property:
[`fullscreen` → `standalone` → `minimal-ui` → `browser`](https://developer.mozilla.org/en-US/docs/Web/Progressive_web_apps/Manifest/Reference/display),
each level removing progressively less chrome
([web.dev](https://web.dev/articles/fullscreen)). The design target here is the `fullscreen` end of
that chain: the surface occupies **100vw × 100vh**, and the controls do not live in a frame that
reserves screen space by shrinking the canvas — they float.

**Full-bleed is earned, not free.** Android's own guidance is unusually honest about the cost:
immersive chrome-removal trades away the user's easy access to system navigation, so it is justified
["only when the benefit… goes beyond simply using extra screen space"](https://developer.android.com/design/ui/mobile/guides/layout-and-content/immersive-content).
A spectrum canvas passes that test for exactly the reason a map does: **panning, zooming, hovering and
clicking the data *is* the interaction.** You are not reading a page that happens to be large; you are
manipulating the data directly, so the data deserves every pixel and the controls become things that
float over it. On hackriff's portable-handheld form factor (a PortaPack replacement), reclaiming the
frame's worth of pixels is not cosmetic — it is most of the usable screen.

**A frame shrinks content; an overlay floats above it.** This is the concrete geometric difference
between the current MUI shell and the target. Today the Explore/Decode app-shell *frames* the canvas:
a left inventory sidebar, a right focus panel and a bottom outputs dock stack around it, each
permanently subtracting from canvas area (`docs/14`, the Explore layout). The reframe puts every one
of those controls into the **z-axis** instead — semi-transparent panels docked to the viewport edges,
able to fade when idle, sitting *above* a full-bleed surface. Design-system vocabulary already names
this: Fluent's ["layering"](https://en.wikipedia.org/wiki/Fluent_Design_System) and Material's
["surfaces"](https://m1.material.io/layout/structure.html) both describe a base canvas plus content
that *floats on top*, as a continuous surface or as discrete cards. Google Maps' own 2024 redesigns
pushed even the transport-mode carousel down into a swipeable bottom strip specifically
["to enhance reachability… while keeping the map visible"](https://9to5google.com/2024/07/14/google-maps-android-redesign/) —
the literal precedent for a detail panel that coexists with a live, pannable surface instead of
eating its width.

The MCANVAS invariant this obeys, and must keep obeying: **chrome floats in *screen* space; data
floats in *content* space.** Controls are pinned to the viewport; boxes, pins, the time cursor and
the axes are anchored in capture-time/frequency and re-lay-out every frame through the pane's own
mapping (`docs/16 §8.4a`; the overlay-anchoring invariant in root `CLAUDE.md`). The reframe is a
change to where the *chrome* lives, and touches the data-anchoring rule not at all.

---

## 3. Direct manipulation, and the same gesture everywhere

The interaction principle underneath the whole map grammar is Ben Shneiderman's **direct
manipulation** (1983, formalized 1997). Its four properties
([NN/g](https://www.nngroup.com/articles/direct-manipulation/),
[Shneiderman 1997 PDF](https://www.cs.umd.edu/~ben/papers/Shneiderman1997Direct.pdf)):

1. **Continuous representation** of the object of interest, in its final form;
2. **Physical, gestural actions** (drag, pinch, wheel) instead of typed command syntax;
3. **Rapid, incremental, reversible** operations;
4. whose **effect is immediately visible**.

A waterfall pan/zoom is the canonical case — arguably a purer case than a geographic map, because the
object being manipulated (a time–frequency window over live data) is continuous in both axes and has
no "places" to snap between. Drag pans, wheel zooms, and the view responds every frame. The
consistency demand from the direct-manipulation literature is that **the same gesture must behave the
same way across content** — pinch-to-zoom on a map must feel identical to pinch-to-zoom on a photo
([UX Tigers](https://www.uxtigers.com/post/direct-manipulation)). For hackriff that means the pan/zoom
grammar is one vocabulary over the whole surface: over raw spectrum, over a detection box, over a
cluster of pins. (The minimap this sentence once listed is retired — T-995, 2026-09-25.) A gesture never means one thing here and another thing there.

hackriff already has this vocabulary, specified in [`ui/CONTROLS.md`](../ui/CONTROLS.md) under
"Navigating the surface" (T-456), and this design **adopts it unchanged**:

| Gesture | Effect |
|---|---|
| Drag | Pans both axes |
| Wheel | Zooms both axes, about the cursor |
| Shift + wheel | Zooms frequency (X) only |
| Alt/Option + wheel | Zooms time (Y) only |
| Ctrl/Cmd + wheel, pinch | Uniform zoom |
| Shift + drag | Marks a **region** (T-458), never pans |
| Double-click | Sends the active pane there |

The one authoring gesture the research tooling needs — drag-to-select a region — is already
reconciled with pan by the `Shift + drag` binding (T-458), so adding markers and measurements does
**not** introduce a second, conflicting gesture grammar. §8 keeps that discipline, and
[§10.4](#104-gestures-and-the-tool-mode) settles which of the two alternatives it is: `Shift + drag`
keeps **one meaning in every mode**, and an explicit, visible **tool mode** re-binds only the *bare*
drag — so the canvas keeps one gesture vocabulary and authoring adds no gesture to it.

---

## 4. The line between view and device — the safety model

Direct-manipulation theory draws a sharp line between manipulating the *representation* (reversible,
local, no external effect) and triggering an *irreversible or external* action (explicit, discrete,
separately confirmed). Google Maps encodes this line as the difference between "look around" and "get
directions": you can pan across the whole planet for free, but routing is a deliberate, separate act.

hackriff already encodes the same line, and it is more than an ergonomics nicety here — **it is the
safety model of a device that transmits nothing but tunes a real radio.** The invariant, from root
`CLAUDE.md` and `docs/16 §8.4`:

> **A pan or wheel never commands the radio.** Time is always a view over already-captured data.
> Panning to un-tuned spectrum *offers* a retune; selecting a region *commands* one — a discrete,
> explicit act through the one gated `DeviceAction` path (one capture at a time, settle gap,
> `device_id` recorded), snapped to the nearest achievable config and refused if the view moved.

**The one amendment, and what makes it safe (T-1028, user 2026-09-25).** The user asked for a
**retune mode**: *"they can turn on 'retune' mode and whenever they zoom or pan it retunes to that …
For areas that are too large and can't be tuned, use the largest possible size instead of denying
them … Maybe it's a keyboard thing, hold down a certain key while panning to retune."* So the line
above holds **by default and with the mode off** — the empty-call-list controls all still run — and
inside the mode the view's frequency window *is* the tune request:

- **The mode is the discrete, explicit act.** What T-407/T-444 required of the *press* is now
  required of the *mode*: it is turned on deliberately (a chip in the map controls, or `R` tapped),
  it is visible while it is on (the chip lit, a banner naming what a gesture will now do, a status
  line on the pane), and `R` **held** is the momentary form — release and the old rule is back.
- **A gesture commands once, when it SETTLES** (pointer release / pinch end, or ~150 ms of stillness
  for a wheel, which has no release), through the same gated `DeviceAction` path. The latest settled
  view wins; a request already in flight is never cancelled and the next waits the settle gap. **A
  press that never moved the view is not a gesture** — a click to focus a signal, a tap, a
  long-press, a Pin-mode tap and a cancelled press reach nothing, in the mode or out of it. (T-486's
  follow/pause commit runs at every release and is a different statement from "a gesture happened";
  conflating the two is what let a click retune in T-1028's first cut.)
- **Too wide is not an error here.** A view wider than one capture window tunes the **largest
  achievable span centred on it**, clamped into the tunable range at the band edges. The pane keeps
  showing the wider view and the coverage fog shows which part of it the radio took — the honesty
  tiers are untouched, and nothing claims detail outside the tuned window. The refusal that remains
  is a view with no overlap at all with the tunable range (or a replay's extent).
- **Frequency only.** Time, the pane's follow/frozen state, the ring and detection are never touched:
  a frozen pane in retune mode retunes and stays frozen.

**Full-bleed makes this line more important, not less.** When the canvas is the entire screen and
every control floats over it, there is more surface to drag across and more temptation to treat a
gesture as a command. The design keeps the line bright by keeping the two categories physically
distinct: **view arithmetic in time** (pan, zoom, pause, scrub, split, follow) never touches a device
route, and **a device command in frequency** (retune to reach un-tuned spectrum) is always a separate,
confirmed act. This is precisely the taxonomy of §3 — reversible representation vs external action —
applied to the one place hackriff cannot afford to blur it.

The line stays true the way it already does: with a **spy-client test that asserts an empty call
list.** T-340 and T-442 both exercise the entire gesture vocabulary against a `fetch` spy and assert
zero device calls resulted; T-458 does the same for region strokes (six drags including region
strokes, zero device calls, two selection POSTs). Every control this document adds — layer toggles,
the Explore drawer, pins, the bottom sheet, measurement authoring — is a *view* or a *durable-state*
action, and each must pass the same assertion: **exercising it changes the screen or a research
store, and calls no device route.** The empty call list is not a test detail; it is how the safety
model stays honest as the chrome grows.

---

## 5. Progressive disclosure and the bottom sheet

Jakob Nielsen's **progressive disclosure** (1995, still current NN/g doctrine) is the information-
density principle that lets a full-bleed surface stay uncluttered: sequence the interface so the
initial view shows *what matters most*, and defer secondary detail to on-demand affordances rather
than showing everything at once
([NN/g](https://www.nngroup.com/videos/progressive-disclosure/),
[IxDF](https://ixdf.org/literature/topics/progressive-disclosure)). Shneiderman's mantra states the
same sequence for spatial data: **overview first, zoom and filter, then details-on-demand.**

The **bottom sheet** is the non-modal disclosure vehicle that map UIs converged on precisely because
it resolves the conflict between "show detail" and "don't cover the surface." Material's specs
([M2](https://m2.material.io/components/sheets-bottom),
[M3](https://m3.material.io/components/bottom-sheets/overview)) and
[NN/g](https://www.nngroup.com/articles/bottom-sheet/) describe a draggable panel with discrete snap
states — **peek → half → full** — whose content re-flows per state rather than being hidden wholesale,
and which never fully obstructs the content beneath it. In hackriff:

- **The detail panel becomes a bottom sheet (or an edge slide-in on wide screens).** Clicking a pin
  or a detection box opens it at *half*, showing the selected signal; it drags to *full* for the
  packet inspector or the measurement table, and collapses to a *peek* strip that keeps the canvas
  live and pannable underneath. This rehomes MUI's right-hand focus panel (`docs/14`, the Explore
  right panel) into the map idiom without losing any of its content — big frequency, measurements
  with their measured-at time, ranked explanations, and the action row (Listen, Decode/RDS, Record
  clip, Stream out, Promote/Delete, Analyze).
- **The Explore drawer is progressive disclosure applied to *discovery*** (§ below), collapsed by
  default to a peek strip.
- **Secondary controls live behind a menu, not all visible.** The default floating control set is
  minimal — search/go-to-frequency, the layers button, a follow-live control, zoom affordances.
  Measurement tools, layer management, saved-collection management and the legend are tucked behind a
  menu or the layers panel, disclosed on demand. ArcGIS Hub's UX guidance frames this as the classic
  IA move: minimize the choices exposed at once, and assume the user did not arrive at a canonical
  "front door" ([ArcGIS Hub UX best practices](https://hub.arcgis.com/documents/178d2fd1617d4429a1f63c0f9a1ea5ea)).

**The Explore drawer** deserves its own note, because it is the one wholly new *discovery* affordance.
Google's Explore strip is a collapsed-by-default bottom sheet that surfaces "interesting places you
might want to get into" and coexists with the still-pannable map. hackriff's analogue surfaces
interesting *spectrum*: quiet-but-active bands, recent anomalies, the strongest current signals, and
**past surveys** (jump-to a prior coverage window). It reads only existing routes —
`GET /api/scheduler` (points of interest), `GET /api/events`, `GET /api/coverage`,
`GET /api/analysis/strongest` — and, like every sheet, never modals over the surface.

---

## 6. Semantic zoom = the honesty tiers

**Semantic zoom** changes *what kind* of information is shown as you zoom, not merely the scale of the
same pixels ([Emergent Mind survey](https://www.emergentmind.com/topics/semantic-zoom)). Google Maps
is the everyday example: zoom out and individual businesses collapse into a single labelled district;
zoom in and roads, then building footprints, then street-level detail appear. The literature attaches
one hard **consistency constraint** to it: *features introduced at a given level persist at all deeper
levels* — nothing may vanish and then reappear as you cross a zoom threshold, or the user loses the
thread of what is there.

**hackriff's three honesty tiers *are* semantic zoom.** The `Resolution` tiers already in the pyramid
(`docs/16`, `resolution.source`) — **`live-iq`** (live-IQ detail), **`spectrum-history`**, and
**`survey-overview`** — are exactly a semantic-zoom ladder: each zoom level discloses only the detail
the hardware actually justifies. A wide or deep zoom shows *overview*, not upscaled detail dressed as
measurement. The semantic-zoom literature gives this design two things:

1. **A vocabulary.** The tiers are not a rendering optimization; they are a semantic-zoom scheme, and
   naming them that way is why the "state the level per pane" rule (`docs/16 §8.4a`, T-442's
   `paneStatuses()` and `levelDivergenceNote()`) is the *correct* fix rather than a workaround: an
   honest semantic-zoom UI states which level it drew, because the same energy legitimately reads
   differently at a coarser cell.
2. **A consistency check.** Audit the tiers against the "nothing vanishes then reappears" constraint.
   A detection present at a coarse tier must not disappear at a finer one and come back; grey
   (unobserved) must not flicker to observed and back across a threshold. This is the semantic-zoom
   discipline applied to the coverage map, and it is a concrete acceptance criterion for the tile
   design, not a vibe.

The tiers **must stay visually distinct** — never upscaled measurement dressed as fresh data. This is
the same exploration-first honesty the product applies everywhere (the database only *suggests*; grey
means *genuinely unobserved*), applied to navigation: the UI must never imply detail the front end
cannot deliver (`docs/16 §8.3`, the honesty-tier invariant).

---

## 7. Figure-ground and honest symbology

Cartography's first design law is that **not all information is equal**
([Map Library](https://www.maplibrary.org/1201/visual-hierarchy-in-cartography-design/),
[Esri](https://www.esri.com/arcgis-blog/products/arcgis-pro/mapping/design-principles-for-cartography)).
A readable map needs a clear **figure-ground** split: the base recedes — low contrast, desaturated —
so the thematic overlay reads as the figure. Esri states it directly: *"low visual contrast works best
for basemaps so that the overlaid thematic layers are more visually prominent"*
([figure-ground](https://www.esri.com/arcgis-blog/products/product/mapping/graphic-design-principles-for-mapping-figure-ground-organization)).

For hackriff the mapping is: **the raw spectrum energy is the ground; detections, pins, annotations
and coverage-grey are the figure.** The waterfall is denser data than anything drawn on it, yet the
overlay must dominate. This validates keeping detection boxes, and observed-vs-grey coverage, visually
distinct from the ramp rather than blended into it — the box invariant and the grey invariant are
figure-ground discipline in RF clothing.

**Symbology** follows the cartographic vocabulary of *visual variables* — shape encodes category, hue
encodes category, size encodes quantity — used deliberately, not decoratively. Mapbox's guidance is
that markers must stay ["legible at sizes as small as 11px"](https://www.mapbox.com/insights/map-design-process),
with [collision detection and variable label placement](https://docs.mapbox.com/help/dive-deeper/optimize-map-label-placement/)
so density does not collapse into noise. So hackriff's pins and boxes use a **small, controlled glyph
vocabulary**: shape and hue encode kind and state (candidate / confirmed / unknown emission), size
encodes bandwidth or confidence, and labels collision-avoid and suppress-at-density as the frequency
axis fills.

**State is never encoded in hue alone.** Roughly **8% of men** have red-green colour-vision deficiency
([Esri colorblind guidance](https://www.esri.com/arcgis-blog/products/arcgis-pro/mapping/designing-maps-for-colorblind-readability),
[Salesforce Maps](https://www.salesforce.com/blog/how-we-designed-salesforce-maps-to-be-color-blind-friendly/)),
so candidate/confirmed/unknown and observed/unobserved each carry a **shape or pattern** in addition
to colour, and the palette avoids red-green pairings (prefer blue-orange or luminance-separated
pairs). The SDR prior art gives a good pattern to borrow: OpenWebRX colour-codes bookmarks by
*provenance/trust tier* at a glance — green for bandplan-derived, yellow for shared, blue for personal
([OpenWebRX bookmarks](https://github.com/jketterl/openwebrx/wiki/How-the-bookmarks-work)) — which
maps cleanly onto hackriff's "known-DB suggestion vs blind detection vs personal note" distinction and
keeps the exploration-first rule *visible*: a suggested allocation must never look like a measured
emitter. That distinction, too, needs a non-hue cue to survive CVD.

**Grey stays reserved for genuinely-unobserved**, and that is the point (`docs/16 §8.1`): across 6 GHz
most of the canvas is honestly empty, and the shape of what is *not* grey is the survey. Figure-ground
here has a third term the geographic case lacks — the *absence* of data is itself a figure worth
reading, and the design must not let a base-map fill or an interpolation paint over it.

---

## 8. The two failures to design against

Google Maps' two research weaknesses, restated as hard requirements. Both are **state-model**
problems, and the fix is the same shape in each: separate **ephemeral** exploration state (hover,
transient filter, current viewport) from **durable** research state, give the durable state its own
persistent surface, and make **export** a first-class path. The full data model is
[`docs/25`](25-spectrum-research-workflow.md) (forward reference, speculative until it lands); this section
states the requirements it must meet.

**Failure 1 — no durable collection.** You cannot, in Google Maps, easily build and keep your own
named collection of marks. Requirement: **collections and annotations are first-class durable state.**
Felt — "Figma for maps" — is the structural model: every point/line/polygon a user draws is also a
**row in a real, inspectable, sortable, exportable table**, and a *layer* (a named, toggleable,
shareable collection) is the unit of organization, not an undifferentiated pile of pins
([Felt interface tour](https://help.felt.com/getting-started/tour-the-interface)). kepler.gl
generalizes the same idea as *config-as-data*: a saved workspace is a serializable set of layer
configs, filters and marker records, distinct from the live data stream
([kepler.gl](https://docs.kepler.gl/)). For hackriff: markers, collections and annotations are
backend objects (SigMF-compatible where they annotate IQ, per
[IQEngine](https://github.com/IQEngine/IQEngine)), and **the table and the canvas are two views of one
data, always in sync.**

**Failure 2 — no reusable measurement.** In Google Maps a measurement is a one-shot readout you write
down elsewhere. Requirement: **a measurement is an object with value, unit, place, time and
provenance** — drawn *on* the canvas and kept, not a tooltip that dies on mouse-up. ArcGIS's
measurement widget is the pattern (distance/area with selectable units, kept live on the map:
[ArcGIS Measure](https://pro.arcgis.com/en/pro-app/3.4/help/mapping/navigation/measure.htm)); the
bioacoustics tools are the closer analogue, because they measure a *spectrogram*: Raven Pro lets a
user draw a time-frequency selection box and computes reusable, exportable measurements per selection
([Raven Pro](https://www.ravensoundsoftware.com/software/raven-pro/)), and Inspectrum's Δf/Δt/symbol-
rate cursors are the SDR version. The provenance requirement generalizes hackriff's existing
*provenance-per-detection* invariant to **provenance-per-annotation**: every durable mark records what
view, config and time window produced it, so it stays meaningful outside the moment it was made — the
cautionary case being Observable notebooks' reproducibility failures from implicit hidden state
([Observable](https://observablehq.com/blog/from-data-exploration-to-data-apps-with-observable)).

**Export is not an afterthought.** Every collection, measurement set and annotation layer needs a path
out — file or link, SigMF-adjacent — so the research artifact outlives the session (the Felt sharing
model, the QGIS/ArcGIS print-composer model of preserving state exactly as authored). A spectrum
canvas that is a research instrument is a map that remembers what you found, *and lets you take it
with you.*

---

## 9. What this must not break

The design is an **extension**. Every invariant below is from `docs/16 §8` or root `CLAUDE.md`, and
this document holds each one. This section is the checklist a later ticket is measured against; a
proposal that fails any line is wrong, not a trade-off.

- [ ] **One surface over one (time × frequency) window.** Layers, pins, sheets and the Explore drawer
  are all *views over the current selection*. They never introduce a second coordinate system or a
  second subject; panes remain where you look *from*, not extra subjects (`docs/16 §8.4`).
- [ ] **One shared absolute-time axis.** Every time-varying overlay — boxes, pins, measurements,
  cursor, pane rectangles, the spectrum trace — lays out through the pane's own capture-time mapping,
  **re-laid-out every render frame in the same pass as the data**, never on the data-poll cadence and
  never at fixed screen coordinates. Chrome floats in screen space; data floats in content space (§2).
- [ ] **Grey = genuinely unobserved, and it is the point.** No layer, base style or interpolation may
  paint over unobserved space; observed-but-not-yet-measured and unknown-whether-we-looked stay
  distinct marks, not grey (`docs/16 §8.1`, T-441).
- [ ] **A detection is a time–frequency box `[start, end?]`.** Pins and clusters never fabricate a
  timespan: a sub-pixel burst renders as a marker or a per-cell count (reading the existing
  `GET /api/tiles/events` aggregate), never a fattened box that invents duration (the coarse-zoom
  honesty rule).
- [ ] **Overlap is an error signal.** The overlay layer never stacks competing boxes as a feature;
  overlap remains the trigger for backend re-analysis, drawn as such (ADR-0019).
- [ ] **Navigation = view arithmetic in time + a device command in frequency.** A pan/wheel never
  commands the radio; region-select on un-tuned spectrum does, through the one gated `DeviceAction`
  path. The spy-client empty-call-list test covers every new control (§4).
- [ ] **Pause freezes the view, not the capture.** Per-pane pause is a coordinate change, reaches no
  route, and does not slow the SDR, the ring or detection (`docs/16 §8.4a`, T-347/T-442).
- [ ] **The three honesty tiers stay visually distinct**, each pane states the level it drew at, and
  no zoom fakes resolution the hardware did not capture (§6; `docs/16 §8.3`).
- [ ] **The client stays thin.** All signal logic and all four durable research stores live in the
  backend behind `docs/api.md` and its contract tests (T-079); `ui/src` only renders, maps
  pixels↔(Hz, time), and calls routes. A UI cutover is not evidence a route has no other caller;
  the guard is a `ui/test` assertion of the *request the client builds* (the T-367 lesson).
- [ ] **The view opens on the observed extent from the coverage map** (`surface/bootstrap.ts`), never
  on the whole 1 MHz–6 GHz midpoint and never derived from `frequency.current` (`docs/16 §8`, T-376).
- [ ] **A visible tile is never abandoned** (§10.9, T-1057). Every pending visible address is re-requested
  with jittered backoff until it is served or the route states it does not exist; the coverage survey may
  turn a place grey and may never leave it pending; a retune refreshes the survey before it may veto.

---

## Application summary — where each piece lands

A compact map from this philosophy to the MCANVAS surface, for the ticket doc (`docs/26`) to expand.
Routes marked *(new)* are the only backend additions; everything else reads a route that already
exists.

| Piece | What it is | Reads |
|---|---|---|
| **Chrome reframe** | 100vw×100vh surface; floating, fade-when-idle overlays; detail as a bottom sheet (peek→half→full) | — (client layout over existing state) |
| **Layer registry** | Per-pane, two axes: **base style** (ramp/phosphor, T-475) and **overlay layers**, each a pure function of served data, independent visibility + z-order | `GET /api/tiles`, `/api/coverage`, `/api/events` |
| **Coverage-fog layer** | Grey/unknown/unobserved/excluded as an explicit toggleable figure-ground layer | `GET /api/coverage` |
| **Detections layer** | Confirmed (prominent) + candidate (weaker) boxes per the box invariant, never stacked | `GET /api/events` |
| **Artifacts layer** | Image/harmonic/IMD relationships drawn as *linked* overlays tying a spur to its source | `GET /api/inventory` (artifact-of) |
| **Band-plan priors layer** | Allocations as ranked *explanations* beside measurements — never truth, never pre-populating | *(new)* priors-over-viewport route |
| **Pins** | First-class markers: rest / hover (MapTip readout) / selected (opens sheet); cluster at coarse zoom, resolve on zoom-in | `GET /api/events`, `/api/tiles/events` |
| **Explore drawer** | Collapsed bottom sheet: quiet-but-active bands, anomalies, strongest signals, past surveys | `GET /api/scheduler`, `/api/events`, `/api/coverage`, `/api/analysis/strongest` |
| **HUD axes** | Floating frequency (bottom) + time (left) rulers with ticks+labels, content-anchored, fade with chrome — finishes T-459 | — (client, over served timestamps) |
| **Marker collections** | Named, layer-toggleable collections of time-frequency markers; generalize frequency-only bookmarks | *(new/extended)* collections store |
| **Saved measurements** | Δf / Δt / bandwidth / duration / symbol-rate objects with value+unit+place+time+provenance, drawn on the canvas and kept | *(new)* measurements store |
| **Annotations** | Durable, SigMF-compatible, clickable-to-navigate time-frequency notes | *(new)* annotations store |
| **Saved views** | Named restorable (time × frequency) windows, shareable/exportable | *(new)* views store |

The four durable stores and the band-plan-priors route are the backend surface this design adds; the
authoring gestures reuse `Shift + drag` (T-458) so the canvas keeps one gesture vocabulary; and every
new control passes the spy-client empty-call-list test (§4). The rest is presentation over data the
backend already serves.

---

## 10. The layout spec (normative)

*This section is the contract MAP-01…MAP-05, MAP-13 and MAP-24 build to. The mockup
[`ui/mockups/map-ui-v1.html`](../ui/mockups/map-ui-v1.html) is the visual reference; where the two
disagree, this section wins and the mockup is a bug report. Rationale is
[ADR-0023](adr/0023-map-ui-and-research-state.md) §1 and §7. The user's five principles in §10.6
override the rest of this section where they conflict.*

### 10.1 Four z-bands, and the band decides the coordinate system

The canvas is `position: fixed; inset: 0` — **100 vw x 100 vh** — and no chrome subtracts from it.
Everything else sits in exactly one band:

| Band | z | Space | Members | Laid out |
|---|---|---|---|---|
| **0** | `0` | **content** | the one `<canvas>`: tiles, traces, coverage plane, every overlay stroke, HUD ticks | **every render frame** |
| **1** | `10` | **content** | `#pins` - focusable marks anchored in (capture time, Hz); the active pane's outline (§10.7); **each pane's own Live/Freeze button** (T-1001), all placed from the same frame's pane rectangles | **every render frame, in the same pass as band 0** |
| **2** | `20` | screen | Go-to/search, layers button + panel, tool buttons, zoom cluster, pane-status readout, HUD axis *labels* | on interaction |
| **3** | `30` | screen | the bottom sheet; the Research slide-in | on interaction |
| **4** | `40` | screen | transients: MapTip, retune offer, mode banner, error toasts | on interaction |

Two rules make the table load-bearing rather than decorative:

- **Band 1 is the only content-anchored DOM, and it is laid out inside the render frame.** Positioning
  a band-1 element from a data poll, a timer or a `MutationObserver` re-creates T-388 (per-poll boxes
  against a per-frame scroll) and is a defect, not a style choice.
- **HUD ticks are band 0; HUD labels are band 2.** A tick is data geometry and belongs in the stroke
  pass; a label is text that must stay crisp at any device-pixel ratio and selectable. Both are placed
  from the *same* per-frame capture-time mapping, so they cannot drift apart.

### 10.2 Chrome docking, fade, and what fade may never hide

Chrome docks to viewport edges as floating translucent panels: Go-to top-left; layers / tools /
Research top-right; zoom right; pane status bottom-left. (T-1001: follow-live is no longer chrome at
all — each pane carries its own Live/Freeze button inside its rectangle, in band 1.)
Chrome **fades to ~35 % opacity after ~6 s idle** and returns on any pointer, key or focus event.
**No overlay is draggable or repositionable; users choose visibility only** (§10.6 rule 3): each
dock above is the one position this section gives it, no dock position is read from or written to
local or user state, and a reload restores which panels are shown, never where (guarded by
`ui/test/map-overlay-position.test.ts`, T-896, part of T-825's suite).

**Fade never applies to:** the bottom sheet, the Research slide-in, an open menu, a focused control,
the retune offer, the mode banner, or any honesty statement (the per-pane tier/level readout, the
retention-bound and IQ-horizon rules and the words beside them). A statement about what the data *is*
may not be made less legible to make the picture prettier.

**Collapsing is not fading, and the line between them (T-919, 2026-09-25).** The pane status
bottom-left is a **compact line**, not a panel: the per-pane **tier/level** readout (with that
pane's Retune and the sentence naming where it would go) and the **colour-scale** statement stay on
the picture in every state, never faded and never behind a press. The paragraph-length statements —
the spectrum-trace readout, the IQ-ring rules **in words** (retention bound, oldest IQ, whether this
pane's own time position has IQ), the fog note, the ranked priors and the orientation note — sit
behind a visible toggle with a visible dismiss, because P1 (§10.6 rule 1) says an overlay's default
state is its smallest. Two things this does **not** license: the rules themselves are drawn on every
pane whatever the status says (they are band-0/1 marks, not chrome), and the box can never be closed
to *nothing* — dismissing returns it to the line. Measured: 560 × 184 px permanent, before; 560 × 29
at 1440 px and 340 × 51 at 420 px, after (`ui/e2e/app-status.e2e.mjs`).

### 10.3 The sheet at every width; Research as a right slide-in

Settled 2026-09-22 (ADR-0023 §7); these were the mockup's two open choices.

- **Bottom sheet, every width.** Three snap states - `peek` (a title strip, ~56 px), `half` (~45 vh),
  `full` (~90 vh) - draggable by its grab handle and by flick, with keyboard equivalents. It is
  **never modal**: the canvas beneath stays live, pannable and zoomable, and a pointer event that
  starts outside the sheet reaches the canvas. On viewports wider than ~900 px the sheet is
  width-capped (~520 px) and docked bottom-left, so the centre of the surface is never covered. It
  hosts two tabs: **Explore** (MAP-14/15) and **Selected** (MAP-04).
- **Research is a right slide-in** (MAP-21), not a sheet state. The sheet is *selection-scoped and
  ephemeral*; Research is *durable and cross-window* - docs/25 §1 requires that difference to be
  visible, and "click a row -> the mark selects -> the detail sheet fills" requires both to be open at
  once. Width ~460 px, full height, dismissible; at phone width it becomes a full-height panel and the
  sheet drops to `peek`.
- **Per-viewer state only.** Sheet snap state, Research open/closed and tab, layer visibility and base
  style live in `localStorage` behind `try/catch`, and every surface must render correctly with
  storage unavailable.

### 10.4 Gestures and the tool mode

`ui/CONTROLS.md` (T-456/T-458) is unchanged and remains the one vocabulary. MMAP adds **tool modes**,
which re-bind only the **bare** drag/click and are always visible (a pressed button, a cursor change,
and a banner naming what a drag will do):

| Gesture | Navigate (default) | Measure | Annotate | Pin |
|---|---|---|---|---|
| bare drag | pan both axes | lay measurement cursors | draw an annotation box | - |
| bare click | select / deselect a mark | - | drop a text note | drop a marker |
| `Shift + drag` | **mark a region** | **mark a region** | **mark a region** | **mark a region** |
| wheel, `Shift`/`Alt`/`Ctrl` + wheel, pinch | zoom, per `ui/CONTROLS.md` | unchanged | unchanged | unchanged |
| `Esc` | - | -> Navigate | -> Navigate | -> Navigate |

**Retune mode is not a tool mode** (T-1028): it re-binds no gesture — a bare drag still pans, a
wheel still zooms, `Shift + drag` still marks a region — it changes what the view **coming to rest**
means. So it composes with the table above rather than occupying a column of it, and it has its own
chip and banner. `R` tapped latches it; `R` held is the mode for exactly one gesture. It is off by
default, and `Esc` is not its exit (the chip and `R` are), because it is not a state a bare drag can
have entered by accident.

`Shift + drag` therefore **never changes meaning**, and a region it marks is the input to every action
that needs an extent - the retune offer (T-444), "measure this", "annotate this" - so authoring is
reachable without ever entering a mode. This closes T-445's open capability #2.

**Nothing in this section reaches a device route.** Pan, wheel, pinch, pause, scrub, split, follow,
every layer toggle, every sheet and menu, every tool mode, every authoring action and every research
write must leave the spy-client call list **empty** (MAP-25). The exceptions are two, both explicit
and both through the one gated `DeviceAction` path: a pan to un-tuned *frequency* **offers** a retune
and an explicit press commits one; and, with **retune mode** on (§4's amendment, T-1028), a settled
pan/zoom commits one. With the mode off, MAP-25's empty call list is required of the whole gesture
vocabulary exactly as before, and that is the control the mode's own tests are written against.

### 10.5 Responsive, touch and accessibility floors

- Usable to **400 px** wide with **no horizontal page scroll**; one-handed reach for the sheet, each
  pane's Live button and the tool buttons.
- **Touch:** pinch = zoom (view), two-finger drag = pan (view), long-press = MapTip, region select =
  the retune *offer*. Touch never crosses the view/device line by accident.
- **Hit targets >= 24 px**; pin glyphs >= 11 px with a >= 24 px hit area.
- Every hover behaviour has a keyboard equivalent (focus = MapTip, `Enter` = select), every floating
  panel is reachable by tab order, and focus is visible (`:focus-visible`).
- **State is never encoded in hue alone** (§7; docs/24 §8): candidate/confirmed/unknown and
  observed/unobserved/unknown/excluded each carry a shape or pattern; red-green pairings are avoided.

### 10.6 The user's five map-UI principles (normative, 2026-09-24)

*Stated by the user on 2026-09-24 after reviewing T-801/T-802/T-803 on staging. Where anything else in
this document, ADR-0023 or the mockup disagrees, these win. An audit on the same day found each only
partly planned; the tickets that close the gaps are named per principle.*

1. **Minimize overlay; expose as much map as possible.** An overlay is temporary: it exists to be
   **closed**, returning its pixels to the map. Every band-2/3 overlay therefore has a visible
   **dismiss**; §10.2's fade-to-35 % is an idle courtesy for band-2 chrome, **never a substitute for
   closing**. A panel's default state is its smallest: no list keeps a column of the map, and
   expanded it is an overlay with a dismiss — a column that keeps the height it had before the
   redesign is not minimal. (Closeability across the sheet and the Research slide-in: the
   closeable-overlays ticket, amending T-824.)
   **T-997 (user, 2026-09-25) finished this for the inventory.** T-895's answer was a chip at the
   LEFT EDGE, MID-HEIGHT — a hamburger plus "1 cand, 1 conf" — which the user rejected: "I don't like
   where this is placed, in the vertical center, it overlaps the timeline markers and isn't very
   useful." Two lessons, both general: a floating puck at mid-height belongs to no cluster, and the
   left edge at mid-height is **the time ruler's own column** (the HUD axes print the time labels
   there). The counts are now two small pills docked in the chrome cluster **under Go-to**, each
   opening the bottom sheet **on its list**, and the lists themselves are a section of that sheet —
   so the map's left edge carries nothing but its ruler, and the one panel over the canvas is the
   sheet. The HUD enforces the other half: a time label that would print into the top-left chrome's
   box is **dropped** (`surface/hud.ts`'s `HudReserve`), never printed underneath a control — a
   label under chrome is a label lost, the same defect one row further up.
2. **Anything with coordinates is drawn on the map.** A thing with a (time x frequency) place is
   rendered on the surface — as a point or pin (docs/24), a box, or a **traced path** (an ordered
   (t, f) polyline, like a directions line) — laid out through the pane's capture-time mapping in the
   same pass as the tiles (the shared-time-axis invariant, §10.1). Listing it only in a panel is not
   enough. First path producers: chirps, frequency-hop sequences, sweep paths, and the device's retune
   history (the path-layer and retune-history tickets).
3. **We decide the overlay arrangement.** No overlay is draggable or repositionable: each has one
   position this section assigns (§10.1-§10.3), and the user chooses only **whether** it is shown. No
   chrome dock position is read from or written to local or user state. (The sheet's vertical
   peek/half/full snap, T-803, is a size state of a fixed-position panel and does not conflict.) This is
   a deliberate Google-Maps-style choice to try, guarded by T-825.
4. **Size is inversely proportional to influence.** The bigger a panel, the less it may change the
   map. Big panels (the bottom sheet, the Explore drawer, the Research slide-in, the left column) only
   add or remove marks, or shift coordinates slightly. Anything that changes the map in a **major**
   way — retune, zoom, jump the view, switch base style or layers, follow live — is a **small button
   or a small cluster**. So a bare click on a row in a big panel never jumps the map and never offers a
   retune: that action lives on a small, explicit per-row button. §11 rule 1 (device routes only from
   Go-to and Selected actions) still holds, and T-825 guards that no device route or view-jump handler
   is bound to a panel body element.
   **Rule 4 applied to the MMAP tickets (re-spec, T-899, 2026-09-24).** These replace the matching
   words in each ticket's acceptance; the tickets' 2026-09-24 notes point here.

   | Ticket | Big panel | A bare row / body click does | The major action lives on |
   |---|---|---|---|
   | T-814 (MAP-14) Explore drawer, to ~90 vh | bottom sheet, Explore tab | selects the item and highlights its mark on the surface (if it has one on screen); nothing else | a small per-row **Go** button: pan/zoom there, or — where no tuned window covers it — raise the ordinary gated retune **offer** (band 4), which itself needs an explicit press |
   | T-815 (MAP-15) past-surveys browse | bottom sheet, Explore tab | selects the survey window and outlines its (time x frequency) extent on the surface | a small per-row **Jump** button that restores the pane to that extent |
   | T-821 (MAP-21) Research slide-in, 460 px full height | Research slide-in | selects the row and highlights its mark (table <-> canvas sync is a *highlight*, not a move); inline edits of a row's own fields are writes, not map changes | small per-row buttons: **Go** (view jump, or the gated retune offer when un-tuned) and, on a saved view, **Restore** |
   | T-804 (MAP-04) Selected tab | bottom sheet, Selected tab | nothing on the body: it is read-only measurements, explanations and liveness | a **compact action cluster** of small buttons (Listen, Decode/RDS, Record clip, Stream out, Analyze, Promote/Delete, any retune) — one row of 36-48 px icon+label buttons or a small overflow menu, never a large surface of full-width action blocks |

   Two things stay allowed on a big panel's body because they do not change the map in a major way:
   **selecting / highlighting** a mark (the focus sheet raising itself to `half` is the panel's own
   size, not the map's), and durable writes that **add or remove** a mark (a new annotation, deleting a
   marker). A **follow-on** change is caught too: a subscriber to the selection (`focus`) may not jump the
   view or reach a device either, or the rule would be defeated one hop away.

   **The guard (T-825's, landed by T-899):** `ui/test/app-panel-influence.test.ts` reads every module
   under `ui/src/app/` except a short named list of non-panels (the band-2 map controls, the canvas, the
   nudge cluster, the decoder workbench — each with its reason), so a new panel file is covered the day
   it lands. Every press handler (click, dblclick, key, pointer/mouse/touch down/up, contextmenu) bound
   to anything but a `<button>`/`<input>`/`<select>`/`<textarea>` — a row, a panel body, a mount's host —
   must name no device route and no view jump, following same-file helpers two calls deep. It is proven
   red on injected violations (a row-click jump, a row-click retune, a keyboard twin, a body listener,
   a jump behind a helper, and a jump swapped into each real Explore row) and green when the same call
   moves onto a per-row button.
5. **The existing small controls are right; keep them.** The +/- zoom cluster, the map-type/layers
   button and the Go-to frequency box (T-802) are the model for rule 4, and are not to be replaced
   or enlarged. (T-1001 moved the follow-live reticle FAB into each pane as a small labelled
   Live/Freeze button — same rule, one per pane instead of one for the hidden active pane.)
6. **The map is GIS, not Google Maps: features are drawn at their true extent; markers are a
   generalization, never the representation** (user, 2026-09-24 19:35, after T-809's pins on
   staging: "a point doesn't represent something meaningful on a waterfall graph. A signal has a
   frequency width and a duration, that's a rectangle"). This is the signal-model invariant (a
   signal is a time-frequency region, ADR-0017/0019) applied to symbolization:
   - **Geometry is the feature's true extent.** Every detection, emitter and event is a polygon in
     (t, f) — its box — drawn in content space through the pane's capture-time mapping (§10.1,
     band 0/1). A single-frame impulse is the only true point, and even it is a thin bar of its
     measured bandwidth. An ongoing signal's box runs to the live edge by assumption; the box is
     drawn at the resolution tier the pane was drawn at.
   - **Scale-dependent generalization.** Only when a box is under ~6 px on screen does it collapse to
     a small symbol at its centre, with the same symbology; once zoom makes it at least that big it
     is the box again. Pins exist only as this generalization.
   - **Symbology by attribute, on the polygon:** Confirmed = solid outline + light fill; Candidate =
     dashed outline, no or very light fill; unexplained = outline + '?' label; curated/human marks =
     a distinct outline style. Class is carried by fill, outline and label — never by glyph shape —
     and is colour-blind safe (T-813).
   - **Identify = hit-test the polygon.** Hover anywhere inside the box shows the MapTip; a click
     selects it (thicker outline, corner handles, the sheet rises); Tab walks features in reading
     order (time, then frequency).
   - **Labels by placement rules**, not tooltips only: where a box is wide enough its label
     (frequency · bandwidth · class) sits inside or just above it, and overlapping labels thin by
     priority (Confirmed > Candidate).
   - **Density, not clustering, at coarse zoom.** Where many features would generalize, a
     density layer (features per cell, from `/api/tiles/events`) replaces numbered cluster bubbles;
     drilling in resolves to boxes.

### 10.7 The active pane, and which chrome is global (normative, T-1000, 2026-09-25)

*From the user's split-view review (2026-09-25): "a common use would be to look at one signal from
the past and the current waterfall". With two panes, most of the chrome acts on ONE of them, and
before this section nothing on screen said which.*

**Principle: chrome that acts on one pane must show which.** A split surface has exactly one
**active pane** — the one last pressed, right-clicked, wheeled or pinched, the one a split just made,
or the one chosen by key — and it is **visible**:

- **An outline on the canvas** around the active pane's rectangle (`.sf-active-pane`), placed every
  render frame from the rectangles the frame was drawn with, and at once — in the same event
  dispatch — when the active pane changes. A neutral light line with a dark halo, so it is never read
  as a signal box's class symbology (§10.6 rule 6). Drawn only while there are **two or more** panes:
  with one there is nothing to disambiguate, and an outline would be overlay with no information in
  it (§10.6 rule 1). Never faded (it says where the chrome's presses land) and never takes the pointer.
- **Every per-pane control names it**, by layout position ("pane 2 of 3", left-to-right then
  top-to-bottom — never an internal id), with the same words the outline carries.

| Chrome | Acts on | Says so how |
|---|---|---|
| **Go-to** (and its retune offer) | the active pane | a "pane N" tag in the box; the input's accessible name |
| **Zoom** +/- | the active pane | a number badge on the stack; the buttons' titles |
| **Layers** (base style, coverage, overlays) | the active pane | a number badge on the button; the menu head and every section heading |
| **Live / Freeze** (T-1001) | **its own pane** — never the active one | it is *inside* that pane's rectangle, and says "Live · pane N" while there are two or more |
| **Viewport menu**: Close, Whole surface | the active pane | the menu head, "Viewport · pane N of M" |
| **Tools** (Measure, Annotate, Pin) | the pane the stroke is made on — which the press makes active | the outline moves to it at the press |
| **Colour scale** | **every pane** (docs/16 §8.5a: one scale, stated) | the layers menu's "every pane" section |
| **Spectrum-trace strip** toggle | every pane | the layers menu's "Every pane" section |
| **Outputs**: Listen, Decode, Record IQ, Stream out | **global** — the selected signal or region, not a pane | no pane name, by design |

**Setting the active pane.** A primary press, a **right-click** (and the context-menu request
itself), a wheel and a pinch on a pane each make it active — a right-click that opens a pane's menu
while the chrome goes on acting on another pane is the defect this section closes. Keys, bare only
(never while typing into a field, never with Ctrl/Cmd/Alt, never on auto-repeat):

| Key | Does |
|---|---|
| `]` / `[` | the next / previous pane becomes active (layout order, wrapping) |
| `1`-`9` | pane N becomes active |
| `L` | toggles Live on the **active** pane — by pressing that pane's own Live button (T-1001), so the key and the button cannot differ |

**Nothing here reaches a device route** (§10.4): which pane is active is view state, and `L` is a
coordinate change on one pane. Guarded by `ui/test/app-active-pane.test.ts` (naming, the notifying
accessor, right-click, the keys, the wiring) and `ui/e2e/app-active-pane.e2e.mjs` (click, right-click
and keys each move the outline and the named chrome in the same event, at 1280 and 400 px).

### 10.8 The inventory is per pane (normative, T-1002, 2026-09-25)

*Same review, the half §10.7 could not reach: the chrome now says which pane it acts on, but the
DATA every pane drew still came from one window.*

**Principle: the inventory is time-scoped to the view, and a pane IS a view** (CLAUDE.md's signal
model, ADR-0017 §2.1). Each pane has its own `(time × frequency)` window, so:

- **Each pane's detections are queried for its own window.** One `GET /api/inventory?state=candidate`
  with that pane's `t0`/`t1` and `f_lo`/`f_hi`, and one `state=confirmed` with that pane's own `at`
  (the instant it shows: its frozen time, or the capture clock's live edge while it follows). A pane
  frozen on a past signal and a pane at the live edge are two windows and two answers.
- **A pane draws its own rows and no others** — the mark boxes, the `detections` overlay, the
  artefact links and the pins over them. A pane that has not been answered yet draws **nothing**
  rather than borrowing another pane's: a box is a claim about *this* pane's window.
- **The lists follow the active pane and name it.** The Candidate/Confirmed lists (and the focus
  panel and the menus, which read the same rows) are the ACTIVE pane's — so pressing pane 1 re-scopes
  them in the same dispatch as the outline — and the heading and each tab's accessible name carry the
  same words the outline does ("Candidates · pane 1 of 2"), nothing extra with one pane open (§10.6
  rule 1).
- **Touching one pane moves nothing in another.** Scrubbing, freezing or re-activating pane 1 changes
  pane 1's query and the lists' scope; pane 2 goes on asking about the live edge and its boxes do not
  move. The queries are re-asked when **any** pane's window moves, a pane opens or closes, or the
  active pane changes — not on the time cursor alone, which is only the active pane's.

Still no device route, and no new route at all: the same `/api/inventory` read, asked once per pane.
Guarded by `ui/test/app-pane-inventory.test.ts` (the requests the client builds, per pane) and
`ui/e2e/app-pane-inventory.e2e.mjs` (two windows asked about from one page, pane 2 advancing while
pane 1 sits in the past, the lists named, at 1280 and 400 px).

---

### 10.9 A visible tile is never abandoned (normative, T-1057, 2026-09-25)

*The user, via the supervisor: "Sometimes there are black bars in the waterfall, representing tiles that
haven't been loaded yet; sometimes those never load. If a tile fails to load at all it should be
re-requested. It seems like they are getting abandoned. Left alone long enough, all tiles on the screen
should load. I don't think we should ever see the black tiles."*

**The invariant.** Every pending **visible** address is re-requested, with jittered backoff, **until it
is served or the route states the place does not exist**. Nothing else ends the asking. Three corollaries,
each of which was a live defect:

1. **Only "there is no such node" is permanent.** `hk-api` says that with a **404** (`tiles.rs`: *"scheme
   … has no node at (level_f …, level_t …)"*). Every other refusal — a `400`, a `500`, a `501`, an
   unreadable body, a batch answer that named no entry for the address — is about **this place at this
   moment**, and goes on a **per-address** jittered ladder (`ui/src/surface/retry.ts`: 500 ms doubling to
   30 s, the same ladder shape as `controls/backoff.ts`, additive jitter so a herd cannot re-arrive on one
   tick). T-479's rule that every status the route could emit is terminal was too wide by exactly one
   notch: the route uses `400` both for an address that can never exist *and* for a tile whose level
   cannot be folded **yet**, so one frame of the second meaning cost that place for the rest of the session.
2. **The coverage survey may turn a place GREY; it may never leave it PENDING.** T-580/T-905's
   short-circuit (never-swept spectrum costs no round trip) is granted only where the survey will
   actually draw grey for the place. A survey that settles a place while having no evidence *inside* it
   draws nothing and also stops every lane requesting it — neither grey nor a tile, which is the black bar
   arrived at from the coverage side.
3. **A retune refreshes the survey before it may veto a request.** A survey taken before the radio moved
   cannot speak for any instant after it, so a place that **reaches past** the retune is owed a request
   until a survey whose evidence reaches past it lands — and the retune asks for that survey at once,
   because a surface with nothing following the live edge never asks for another one on its own
   (`preview.ts`'s cadence is `POSITIVE_INFINITY` there). Places that end **before** the retune keep the
   veto and the saving: the radio cannot retroactively have sampled a band it was not tuned to.

**Never at frame rate, either.** The flood T-479 fixed (157 requests in 700 ms for one place) is
prevented by the **wait**, not by permanence: a place is asked at most once per interval, and only while
something draws it — the ladder issues nothing, so a place nobody is looking at is never re-asked. The
cost of a permanently-refused visible place is therefore two requests a minute; the cost of the other
mistake is a bar of the waterfall that stays black until the page is reloaded. Those are not symmetric.

Guarded by `ui/test/surface-tile-never-abandoned.test.ts` (30 % of answers dropped/refused/unreadable at
random on a seeded schedule, every visible address served in the end, no PENDING or REFUSED quad left in
the final frame's draw list; the pacing bound; the ladder's arithmetic; the survey and retune rules; the
batch layer's own entries) and `ui/test/surface-cache.test.ts` (the status enumeration, place by place).

---

## 11. Panel -> state -> route: the frontend/API map (normative)

*This is the ADR-0013 frontend-to-API-map discipline applied to every new MMAP surface. A panel not in
this table has no home; a route in the **Reads/Writes** columns that does not exist yet is named with
its owning ticket and is reserved in [`docs/api.md`](api.md). The client slices are specified in
[docs/24 §15](24-canvas-as-data-surface.md).*

| Panel / surface | Ticket | Client slice | Reads | Writes |
|---|---|---|---|---|
| Full-bleed shell, z-bands | MAP-01 | `map.chrome` | - | - |
| Go-to frequency / search | MAP-02 | `map.chrome` | `GET /api/navigation` (achievable grid) | `POST /api/control/center` **only on explicit press** (device action) |
| Layers button + panel | MAP-02/06 | `layers` | - | - (per-pane presentation; `PUT /api/collections/{id}` only when toggling a *collection's* stored visibility) |
| Per-pane Live/Freeze (T-1001), zoom cluster | MAP-02 | `map.chrome` + the pane model | - | - (pure view arithmetic) |
| Candidate / Confirmed lists + selections **in the bottom sheet**, opened by two count pills in the top-left chrome | T-895, redesigned by T-997 (P1) | `explore` (existing `inventory` / `selections` slices; a pill writes only `inventory.tab` and raises the sheet) | `GET /api/inventory?state=candidate\|confirmed` (**one pair of queries per pane**, each carrying that pane's own window — T-1002, §10.8; the lists show the ACTIVE pane's and name it), `GET /api/streams` + `/ws/presence`, `GET /api/coverage` (empty-list wording); the pills' counts are the same rendered rows, no extra read | - new (a row's Promote/Delete keep the existing `POST /api/inventory/{id}/promote`, `DELETE /api/inventory/{id}`; opening a list and the counts reach no route) |
| Bottom sheet - Explore tab | MAP-03/14/15 | `map.sheet` | `GET /api/scheduler`, `/api/events`, `/api/coverage`, `/api/analysis/strongest`, `/api/observations` (the past-surveys pages), `/api/history` (served; no client reads it since T-445 retired the spectrum-grid pane) | - |
| Bottom sheet - Selected tab | MAP-04 | `map.selection` | `GET /api/inventory/{id}`, `/api/inventory/{id}/presence`, `/api/inventory/{id}/classification`, `/api/signatures/match`, `/api/recipes/match` | `POST /api/analyze`, `POST /api/inventory/{id}/promote`, `DELETE /api/inventory/{id}`, `POST /api/outputs/record/start`, `/ws/open/listen` - **only from the compact action cluster's small buttons, never the sheet body** (§10.6 rule 4) |
| HUD axes (ticks + labels) | MAP-05 | - (pane model) | `GET /api/tiles` `axes`/`extent`, `GET /api/timeline` `window` | - |
| Coverage-fog layer | MAP-07 | `layers` | `GET /api/coverage`, the tile state plane | - |
| Detections layer | MAP-08 | `explore` (existing rows, **per pane** - T-1002 §10.8) | `GET /api/events` | - |
| Pins + clusters | MAP-09/10 | `map.pins` (ephemeral) | `GET /api/events`, `GET /api/tiles/events` | - |
| Artifacts layer | MAP-11 | `explore` (existing rows) | `GET /api/inventory` (`relation`) | - |
| Band-plan priors layer | MAP-12 | `priors` | **`GET /api/priors`** *(reserved - MAP-12)* | - |
| Research slide-in - Markers | MAP-21 | `research.collections`, `research.markers` | **`GET /api/collections`, `/api/collections/{id}/markers`**, and **`GET /api/markers`** (the same list over every collection - what the canvas layer actually asks for) *(reserved - MAP-17)* | **`POST /api/collections`, `POST /api/collections/{id}/markers`, `PUT`/`DELETE /api/markers/{id}`, `PUT`/`DELETE /api/collections/{id}`** |
| Research slide-in - Measurements | MAP-21/22 | `research.measurements` | **`GET /api/measurements`** *(reserved - MAP-18)* | **`POST /api/measurements`, `PUT`/`DELETE /api/measurements/{id}`** - cursors only, never a `value` |
| Research slide-in - Annotations | MAP-20/21 | `research.annotations` | **`GET /api/annotations`** *(reserved - MAP-16)* | **`POST /api/annotations`, `PUT`/`DELETE /api/annotations/{id}`** |
| Research slide-in - Views | MAP-19/21 | `research.views` | **`GET /api/views`** *(reserved - MAP-19)* | **`POST /api/views`, `PUT`/`DELETE /api/views/{id}`**; restoring is view arithmetic, and only a frequency outside the tuned window raises the usual gated retune offer |
| Scan plan overlay (small Scan button in the Go-to cluster; plan panel; the `scan` layer) | T-1008 | `map.scan` (the controller's plan/draft; progress from the shell's existing control-state poll, `device.scan`) | `GET /api/control/scan?windows=1` (the price of the plan as drawn **and the steps the engine will take** — the client draws them, never tiles a range itself) | `POST /api/control/scan` **only on the panel's explicit Start/Resume press** (commissions retunes, T-452), `POST /api/control/scan/stop` (surrenders the radio; never refused) |
| Measurement box menu (right-click / long-press a Measure box) | T-1009 | `map.scan` (the plan it opens) + `research` (the marker it writes) | `GET /api/collections` (which collection a marker is filed in), `GET /api/control/scan?windows=1&device_id=…` (the plan it opens, priced on the chosen radio) | `POST /api/iqbuffer/clip` (the box's time window, band-filtered, off the chosen radio's ring — a read of the ring, not a device route), `POST /api/collections` + `POST /api/collections/{id}/markers` (Save as marker). **Scan** only *opens* the plan overlay above — Start stays that panel's own explicit press |
| Export menu | MAP-23 | `research` | **`GET /api/research/export`** (T-823: one read-only GET, optionally narrowed to a collection - the bundle is the server's) | - (the client only names and saves the file; a share link is a later addition) |

**Three rules this table encodes.**

1. **Only three rows in the whole table reach a device route**, and each needs an explicit press on a
   **small** control: Go-to, the Selected tab's compact action cluster (§10.6 rule 4), and the scan
   plan's Start (T-1008 — a *commission* of retunes, T-452, taken on its own press; opening, dragging
   and pricing the plan are reads). The measurement-box menu (T-1009) is **not** a fourth: choosing
   "Scan this region with <device>" opens that same plan overlay, bounded by the box and bound to the
   radio the user named, and the commissioning press is still the panel's own Start. A per-row
   **Go** button in the Explore drawer or Research may *raise* the gated retune offer, which is itself
   the band-4 transient that needs its own press. Everything else is a view change or a durable-state write.
2. **Every reserved route is named with its ticket and appears in `docs/api.md` before its client
   exists.** The client is thin because the route is the contract, not because the panel is small.
3. **The guard is the request the client *builds*.** Contract tests prove the server serves a route
   correctly; they cannot catch a client asking for the wrong thing (T-367 requested `/api/timeline`
   with no band and drew an empty canvas while every suite stayed green). Every row above owes a
   `ui/test` assertion on the request it constructs (MAP-25).

   **Landed as `ui/test/map-request-shape.test.ts` (T-825).** It parses this table, drives each
   client's own request builder, and asserts every built path against the table: a route the client
   builds that this table does not declare is red, a declared route with neither an assertion here
   nor a named assertion elsewhere is red, and a route pinned as "no client yet" is red the day a
   client starts building it. That is why the table is normative rather than descriptive.

---

## Sources

Grouped by the five research lenses. All URLs accessed 2026-09-21.

**Maps philosophy — chrome, direct manipulation, disclosure, layers**
- Direct Manipulation (NN/g) — https://www.nngroup.com/articles/direct-manipulation/
- Direct Manipulation (UX Tigers) — https://www.uxtigers.com/post/direct-manipulation
- Shneiderman, *Direct Manipulation for Comprehensible, Predictable and Controllable UIs* (1997, PDF) — https://www.cs.umd.edu/~ben/papers/Shneiderman1997Direct.pdf
- Progressive Disclosure (NN/g) — https://www.nngroup.com/videos/progressive-disclosure/
- What is Progressive Disclosure? (IxDF) — https://ixdf.org/literature/topics/progressive-disclosure
- Bottom sheets (Material Design M2) — https://m2.material.io/components/sheets-bottom
- Bottom sheets (Material Design M3) — https://m3.material.io/components/bottom-sheets/overview
- Bottom Sheets: Definition and UX Guidelines (NN/g) — https://www.nngroup.com/articles/bottom-sheet/
- Google Maps directions/sheets redesign (9to5Google, Jul 2024) — https://9to5google.com/2024/07/14/google-maps-android-redesign/
- Immersive content (Android Developers) — https://developer.android.com/design/ui/mobile/guides/layout-and-content/immersive-content
- Making Fullscreen Experiences (web.dev) — https://web.dev/articles/fullscreen
- `display` (Web app manifest, MDN) — https://developer.mozilla.org/en-US/docs/Web/Progressive_web_apps/Manifest/Reference/display
- Fluent Design System (Wikipedia) — https://en.wikipedia.org/wiki/Fluent_Design_System
- Layout structure (Material Design 1) — https://m1.material.io/layout/structure.html
- Layers | Maps JavaScript API (Google) — https://developers.google.com/maps/documentation/javascript/layers
- Data Layer | Maps JavaScript API (Google) — https://developers.google.com/maps/documentation/javascript/datalayer

**Data-surface — sources/layers architecture, pins, clustering**
- Mapbox GL JS: create and style clusters — https://docs.mapbox.com/mapbox-gl-js/example/cluster/
- deck.gl documentation — https://deck.gl/docs
- kepler.gl documentation — https://docs.kepler.gl/
- keplergl/kepler.gl (GitHub) — https://github.com/keplergl/kepler.gl
- Marker — Map UI Patterns — https://mapuipatterns.com/marker/
- Advanced markers: accessibility (Google) — https://developers.google.com/maps/documentation/javascript/advanced-markers/accessible-markers
- Use layers to find places, traffic, terrain… (Google Maps Help) — https://support.google.com/maps/answer/3092439
- Controls | Maps JavaScript API (Google) — https://developers.google.com/maps/documentation/javascript/controls

**GIS-UX — figure-ground, symbology, semantic zoom, accessibility**
- Visual Hierarchy in Cartography (Map Library) — https://www.maplibrary.org/1201/visual-hierarchy-in-cartography-design/
- Design Principles for Cartography (Esri) — https://www.esri.com/arcgis-blog/products/arcgis-pro/mapping/design-principles-for-cartography
- Figure-ground Organization (Esri) — https://www.esri.com/arcgis-blog/products/product/mapping/graphic-design-principles-for-mapping-figure-ground-organization
- Guide to map design (Mapbox) — https://www.mapbox.com/insights/map-design-process
- Optimize map label placement (Mapbox) — https://docs.mapbox.com/help/dive-deeper/optimize-map-label-placement/
- Semantic Zoom (Emergent Mind) — https://www.emergentmind.com/topics/semantic-zoom
- Designing Maps for Colorblind Readability (Esri) — https://www.esri.com/arcgis-blog/products/arcgis-pro/mapping/designing-maps-for-colorblind-readability
- How We Designed Salesforce Maps to be Color Blind-Friendly — https://www.salesforce.com/blog/how-we-designed-salesforce-maps-to-be-color-blind-friendly/
- UX Best Practices (ArcGIS Hub) — https://hub.arcgis.com/documents/178d2fd1617d4429a1f63c0f9a1ea5ea
- Map UI Design (Eleken) — https://www.eleken.co/blog-posts/map-ui-design

**Research tooling — durable collections, measurements, annotations**
- Tour the interface (Felt Help Center) — https://help.felt.com/getting-started/tour-the-interface
- kepler.gl user guides — https://docs.kepler.gl/docs/user-guides
- Measure (ArcGIS Pro) — https://pro.arcgis.com/en/pro-app/3.4/help/mapping/navigation/measure.htm
- IQEngine (GitHub) — https://github.com/IQEngine/IQEngine
- From data exploration to data apps (Observable) — https://observablehq.com/blog/from-data-exploration-to-data-apps-with-observable

**SDR-waterfall prior art — rendering, markers, spectrogram research tools**
- Maia SDR — about / waterfall rendering — https://maia-sdr.org/about/
- SDRangel spectrum markers — https://github.com/f4exb/sdrangel/blob/master/sdrgui/gui/spectrummarkers.md
- SigDigger 0.3.0 (collapsible panel UI) — https://batchdrake.github.io/sigdigger-0.3.0/
- OpenWebRX: how bookmarks work — https://github.com/jketterl/openwebrx/wiki/How-the-bookmarks-work
- Raven Pro (Cornell Lab of Ornithology) — https://www.ravensoundsoftware.com/software/raven-pro/
- SPACE: SPectrogram Analysis and Cataloguing Environment (arXiv) — https://arxiv.org/pdf/2207.12454

**Internal (this repo)**
- `docs/14-ui-rewrite.md` — the MUI thin-client rewrite (Explore/Decode, focus panel, boxes)
- `docs/16-coverage-tile-pyramid.md` §8 — the unified full-spectrum canvas (MCANVAS)
- `ui/CONTROLS.md` — the surface gesture vocabulary (T-456/T-458)
- `docs/api.md` — the thin-client route contract (T-079)
