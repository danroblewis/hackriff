# Spectrum research workflow: collections, measurements, annotations

**Status: SPEC (T-800 / MAP-00, 2026-09-22) — §10 is the normative store contract MAP-16…MAP-19
build to; §1–§9 are the argument behind it.** Design brief 2026-09-21, approved 2026-09-22; decision
record [ADR-0023](adr/0023-map-ui-and-research-state.md). This extends the unified canvas
([docs/16 §8, "MCANVAS"](16-coverage-tile-pyramid.md)) and the UI rewrite
([docs/14](14-ui-rewrite.md)); it contradicts neither and reuses their invariants throughout. It
specifies the **durable research state** that turns the canvas from a viewer into a research
instrument — the fix for the two things the user faults Google Maps for: *you cannot build a durable
collection of your own marks, and you cannot reuse a measurement.* Both are backend stores behind
[`docs/api.md`](api.md) with thin-client surfaces, exactly as every other capability in this product
is.

> This document is the **state-model** half of the Google-Maps redesign. Its sibling, the
> **chrome/layers/pins** half (full-bleed surface, the layer registry, first-class pins, the Explore
> drawer, the bottom-sheet detail panel, the HUD axes), lives in [docs/24](24-canvas-as-data-surface.md) and
> the MCANVAS follow-ups it schedules. The reframe there is why the research tooling here has a place
> to live; the tooling here is why the reframe is worth doing. They are one milestone.

The framing, in one line: *a spectrum canvas that is a research instrument is a map that remembers
what you found.* Google Maps and its consumer peers are optimised for a single-shot question —
"where is X", "how do I get to Y" — and actively resist becoming a workspace: every mark you make is
ephemeral, scoped to one session, disconnected from any narrative you are building
([research-tooling lens](#sources)). The tools that succeed as research instruments — Felt,
kepler.gl, ArcGIS, IQEngine, Raven Pro — instead treat three things as durable, first-class objects
rather than throwaway UI state: **collections, measurements, and annotations**, each with its own
persistence, provenance and export path, sitting alongside (not replacing) the exploratory
pan/zoom/hover the canvas already gets right.

---

## §1. Ephemeral vs durable state

The cross-cutting pattern across every research-grade mapping and signal tool is a hard split
between two kinds of state:

- **Ephemeral exploration state** — the hover readout, a transient brush/filter, the current
  viewport, a drag in progress. It answers *what am I looking at right now*, it is cheap, and it is
  gone the instant the pointer moves or the view scrolls. In hackriff this is already almost
  everything the canvas does: the pane's own `(center_f, span_f, center_t, span_t)`, the
  `Waterfall.boxAt` readout resolved at the pointer's own row time ([docs/14, T-362](14-ui-rewrite.md)),
  the max-hold trace, the pane's pause window.
- **Durable research state** — named collections, saved measurements carrying units and provenance,
  annotations that are real exportable rows, saved views. It answers *what did I find, and can I get
  it back next week.* It survives a restart, it is time-scoped only when the user asks, and it is the
  thing Google Maps does not have.

The design law, drawn from kepler.gl's brushing filter versus ArcGIS's bookmarks and from Felt's
per-layer edit model, is that **the two must be visually distinct so a hover-readout never
masquerades as a saved fact** ([research-tooling lens](#sources); ArcGIS bookmarks,
[Esri](https://pro.arcgis.com/en/pro-app/3.4/help/mapping/navigation/bookmarks.htm); kepler.gl
[filters](https://docs.kepler.gl/docs/user-guides/e-filters)). This is the same discipline hackriff
already enforces one level down, between manipulating the *representation* (reversible, local, zero
external effect) and triggering a *device action* (explicit, discrete, separately confirmed —
[docs/14, T-343](14-ui-rewrite.md)). Durable research state is a third register above both, and it
needs its own affordances: a hover MapTip is translucent and follows the pointer; a **saved** marker
is opaque, sits at a fixed capture-time/frequency coordinate, and appears in a list.

**Argument: durable state gets its own persistent surface.** A canvas alone cannot represent durable
state honestly, because a canvas shows *this window* and durable objects outlive any window. The
established answer — Felt's table, ArcGIS's contents pane, Raven Pro's selection table
([research-tooling / sdr-waterfall lenses](#sources)) — is a **table/list surface kept in sync with
the canvas**, where every durable mark is also a row. §7 specifies that surface. The rule it enforces
is the one Felt is built on: *the table and the map are two views of the same data, always in sync* —
an edit in either propagates to the other, and neither is a second source of truth.

This split is not new to hackriff. It is exactly the **Candidate / Confirmed / History** distinction
the signal model already draws (CLAUDE.md; [ADR-0017](adr/0017-time-extent-signal-model.md)):
Candidate is an ephemeral hypothesis about energy in the current window, Confirmed is a durable
verified emitter, History is the durable catalogue. The research tooling adds a *human-authored*
column to that same durable register — collections, measurements, annotations, saved views — that is
never fed back into blind detection (§9). The machine finds emitters; the researcher keeps notes;
the two share geometry and a surface but never each other's authority.

---

## §2. Provenance for every durable object

hackriff already requires **provenance per detection**: every `Detection` carries the gain state,
overload flags, spur masks and capture-clock time under which it was measured
([docs/07](07-data-model.md); the `hackriff:provenance` SigMF block,
[docs/sigmf-extension.md](sigmf-extension.md)). This design **generalises that to provenance per
annotation**: every durable object a researcher creates records what data, view, config and time
produced it.

The cautionary case is Observable notebooks, which are excellent for keeping code, narrative and
results together but empirically suffer reproducibility failures from hidden state and ambiguous
execution order ([research-tooling lens](#sources);
[Observable](https://observablehq.com/blog/from-data-exploration-to-data-apps-with-observable)). A
measurement or annotation with no recorded context is the same trap: a "−93 dBm" or a "looks like a
pager" that cannot be reproduced because nobody knows which gain state, which tune, which second of
capture produced it. The positive case is IQEngine's SigMF annotations, which *are* the shareable
unit precisely because they carry their own coordinates and travel with the recording
([research-tooling / sdr-waterfall lenses](#sources);
[IQEngine](https://github.com/IQEngine/IQEngine)).

**The provenance stamp.** Every durable research object carries a `provenance` object, a subset of
the same shape `hk_model::Provenance` already serialises for detections, plus the view context that
produced the authored mark:

```json
"provenance": {
  "device_id": "hackrf:<serial>",           // which front end's coverage this rests on, or null
  "center_hz": 100300000.0,                  // the tune in force when authored
  "span_hz": 2400000.0,                      // instantaneous span in force
  "sample_rate_hz": 2000000.0,
  "t_capture": [1726480000.0, 1726480012.5], // the CAPTURE-clock window the object refers to
  "tier": "live-iq",                         // "live-iq" | "spectrum-history" | "survey-overview"
  "authored_s": 1726480013.9,                // wall-clock instant of authoring (audit, not measurement)
  "actor": "<token fingerprint>"             // who authored it (never the token itself)
}
```

Three points make this honest rather than decorative:

- **`t_capture` is on the capture clock, `authored_s` is on the wall clock, and they are never
  conflated.** This is the same bug the UI has now found five times (T-379, T-384, T-389, and the
  inspector — [docs/14](14-ui-rewrite.md)): a browser instant compared against capture-clock time is
  wrong by the fixture's offset (3.5 days on the capture that first exposed it). A measurement refers
  to *when the air was*, `t_capture`; the audit trail records *when the human acted*, `authored_s`;
  the two are different quantities and are stored apart.
- **`tier` records the honesty level the object was drawn at.** An annotation authored over a
  `survey-overview` pane refers to reduced, non-live-IQ data, and its provenance says so — the same
  three-tier honesty the canvas already carries ([docs/14, T-341](14-ui-rewrite.md); docs/16 §4). A
  measurement taken off upscaled overview pixels is a weaker claim than one taken off live IQ, and
  its stamp must let a later reader tell the difference.
- **`device_id` scopes the coverage the object rests on**, exactly as a detection's provenance and a
  pane's `device` do (docs/16 §8.4). An artifact-of or a "quiet here" annotation is a claim about
  *one receive chain*, never a universal one (the same rule `hk_model::relate` enforces for images
  and harmonics — [docs/api.md](api.md#get-apiinventory--signal-inventory-t-018-t-078-aware-053aware-042)).

The provenance stamp is written by the **backend** at create time from the request's stated view
context plus the run's own device/coverage state; the client sends the view context it was on, and
the server stamps the rest it authoritatively knows. This keeps the thin-client rule: the UI reports
where it was looking, the backend records what that means.

---

## §3. Marker collections

Today's [`/api/bookmarks`](api.md) is **frequency-only**: a `Bookmark` is
`{id, kind ("marker"|"bookmark"), name, f_center_hz, bandwidth_hz, note, created_s, updated_s}`, with
no time extent and no grouping. That was right for a tune-and-listen bookmark bar; it is wrong for a
canvas whose whole coordinate system is **(time × frequency)** and whose signal model says a signal
is a time-frequency region, not a carrier ([ADR-0017](adr/0017-time-extent-signal-model.md)). A pin
dropped on a one-off 902 MHz burst has a time; a bookmark on the FM band does not. Both must be
expressible.

This design generalises bookmarks into **time-frequency markers grouped into named, toggleable
collections** — the kepler.gl / Felt "a layer is the unit of organisation, not an undifferentiated
pile of pins" model ([data-surface / research-tooling lenses](#sources)).

### The objects

```
Collection: { id, name, note, color, visible, created_s, updated_s, member_count }
Marker:     { id, collection_id, name, note,
              f_center_hz, bandwidth_hz?,          // frequency place (bandwidth optional, as today)
              t_center_s?, duration_s?,            // NEW: time place; null/null = a frequency-only pin
              provenance,                          // §2 stamp
              created_s, updated_s }
```

A marker with `t_center_s = null` is a **frequency-only pin** — a band you always want marked,
drawn as a full-height vertical guide, structurally identical to today's bookmark. A marker with a
time places as a **point or small box** at its capture-time coordinate and moves with the canvas like
every other overlay (docs/16 §8; [docs/14, T-362](14-ui-rewrite.md)). This is the direct analogue of
a Google Maps pin, but on the (time × frequency) plane.

### Routes

```
GET    /api/collections                     -> { "collections": [Collection, …] }
POST   /api/collections                     { "name", "note"?, "color"? }            -> Collection (201)
GET    /api/collections/{id}                -> Collection
PUT    /api/collections/{id}                any create field; { "visible" } toggles   -> Collection
DELETE /api/collections/{id}                -> { "deleted": Collection, "members_deleted": N }

GET    /api/collections/{id}/markers        -> { "markers": [Marker, …] }
POST   /api/collections/{id}/markers        { "name", "f_center_hz", "bandwidth_hz"?,
                                              "t_center_s"?, "duration_s"?, "note"? } -> Marker (201)
GET    /api/markers/{id}                     -> Marker
PUT    /api/markers/{id}                     any create field (null clears optionals)  -> Marker
DELETE /api/markers/{id}                     -> { "deleted": Marker }
```

The route shapes mirror `/api/selections` (persisted named regions that already survive a restart in
the run's database — [docs/api.md](api.md) — and already carry `t_lo`/`t_hi`, `tags`, `notes`).
`GET` is unwindowed and returns the whole collection; the canvas filters to the visible window
client-side, because a collection is durable and not scoped to any view (§1). A `POST` returns `201`;
error codes follow the control API exactly — `invalid` (400), `not_found` (404), `conflict` (409 on
a duplicate supplied id), `unavailable` (503 when the server has no collection store).

### Migration and compatibility with `/api/bookmarks`

Bookmarks are not deleted or broken. Two options, and this design takes the first:

1. **Bookmarks become a reserved built-in collection.** On first start with a collection store, the
   existing bookmarks migrate into a system collection `"Bookmarks"` (id fixed, un-deletable,
   `color` = the current bookmark accent), each bookmark becoming a frequency-only marker
   (`t_center_s = null`), preserving id, name, note and timestamps. `/api/bookmarks*` stays live as a
   **compatibility facade** over that one collection — same shapes, same audit action name — so the
   frequency-only bookmark bar and any external caller keep working unchanged. New clients use
   `/api/collections`; the two see the same rows.
2. *(Rejected)* leave `/api/bookmarks` fully separate and add collections beside it. Two stores for
   the same idea is exactly the "two implementations of one idea drift apart" defect the canvas
   cutover was built to kill (docs/16 §8.5). One store, one facade.

### Toggle-as-a-layer, audit, availability

A collection's `visible` flag is what the layers menu (docs/24) toggles: a collection *is* an overlay
layer, so hiding it is a per-pane visibility toggle over one data source, in the Mapbox/Google-Maps
"layers are independently toggleable" model ([data-surface lens](#sources)). Marker creation,
edit and deletion are **mutating** requests and are audited exactly like `/api/bookmarks*` and
`/api/selections*` today — token id (never the token), peer, action, body, old/new, status
([docs/api.md](api.md)) — and they carry **no `device` key**, because authoring a marker is a view
change and never reaches the radio (§9; the device-action classification in
[docs/api.md](api.md#device-actions-t-343)). Without an audit log, every mutating collection endpoint
answers `503 unavailable`, consistent with the rest of the control API.

---

## §4. Saved measurements

The user's precise complaint about Google Maps: *"It's not easy to use the measurements for anything,
you are more likely to write it down somewhere else."* The fix, from ArcGIS's measurement widget and
Raven Pro's selection measurements, is that **a measurement is a persistent object, not a tooltip
that vanishes on mouse-up** ([research-tooling / sdr-waterfall lenses](#sources);
[ArcGIS Measure](https://pro.arcgis.com/en/pro-app/3.4/help/mapping/navigation/measure.htm); Raven
Pro's 70+ reusable measurements per selection).

A measurement in hackriff is **value + unit + place (a time-frequency extent) + time + provenance**,
drawn on the canvas and kept editable and referenceable. The measurable quantities on a (time ×
frequency) plane, mirroring inspectrum's cursor tools (RESEARCH-003):

- **Δf** — a frequency span between two cursors (Hz).
- **Δt** — a time span between two cursors (s).
- **bandwidth** — the −3 dB or user-marked width of a region (Hz).
- **duration** — the time extent of a burst or region (s).
- **symbol rate / period** — inspectrum-style: the user marks N cycles of a repeating feature and the
  backend returns the rate (baud / Hz) and period (s), the reciprocal pair.

### The object and routes

```
Measurement: { id, collection_id?, kind ("delta_f"|"delta_t"|"bandwidth"|"duration"|"symbol_rate"|"period"),
               value, unit,                        // e.g. 12500.0, "Hz"
               f_lo_hz, f_hi_hz, t0_s, t1_s,        // the PLACE it was measured over
               cursors,                             // the raw cursor coordinates that produced value
               n?,                                  // cycle count for symbol_rate/period
               note?, provenance, created_s, updated_s }

GET    /api/measurements                     ?collection=&f_lo=&f_hi=&t0=&t1=   -> { "measurements": [Measurement, …] }
POST   /api/measurements                     { "kind", "cursors", "collection_id"?, "note"? } -> Measurement (201)
GET    /api/measurements/{id}                -> Measurement
PUT    /api/measurements/{id}                { "cursors"?, "note"?, "collection_id"? }         -> Measurement
DELETE /api/measurements/{id}                -> { "deleted": Measurement }
```

The optional filter parameters on `GET` let the table (§7) scope a query to a window when a
researcher asks; unwindowed it returns all, because a saved measurement is durable.

### Thin-client split: the arithmetic that is presentation vs the stored object

This is the hard line the thin-client rule draws, and it must be drawn precisely for measurements or
signal logic leaks into `ui/src`:

- **Pure presentation (in `ui/src`, allowed):** mapping the two cursor *pixels* to `(Hz, s)`
  coordinates through the pane's existing pixel↔Hz and time↔pixel maps; subtracting two coordinates
  to display a *live* Δf/Δt readout while the user drags; formatting units. This is the same class of
  arithmetic the client already does for the hover readout and the retune-snap
  ([docs/14, T-341/T-343](14-ui-rewrite.md)) — arithmetic over already-known UI state, no signal
  logic.
- **The stored object and any measurement over the data (backend):** the moment a measurement becomes
  *durable* — saved with a value — the backend owns it. Crucially, **symbol rate and −3 dB bandwidth
  are measurements over the IQ/spectrum, not over pixels**: a bandwidth is a function of the noise
  floor and the −3 dB points in the actual data, and a symbol rate marked as "N cycles over this
  span" must be computed from the signal, not from where the cursor happened to land. So `POST
  /api/measurements` sends the **cursors** (the place), and the backend computes `value`/`unit` from
  the data under that place and stamps provenance. A raw Δf/Δt of two frequencies is trivial and the
  backend still owns the stored record so that every measurement has one authority and one provenance
  path — never a value the client computed and the backend merely filed.

The live-drag readout is ephemeral (§1); pressing "save" is what crosses into a durable object, the
same peek→commit boundary as everywhere else. A saved measurement then draws on the canvas as a
dimensioned annotation (cursor rules with a value label), placed in capture time so it moves with the
data, and it is editable: dragging a saved cursor re-`PUT`s it and the backend recomputes.

---

## §5. Annotations

An annotation is a **durable, SigMF-compatible, clickable-to-navigate time-frequency note** — text
or graphical — that a human authors on the canvas. This is the IQEngine model directly: a spectrogram
viewer where annotations are graphical-or-text, clickable to jump to, and streamed incrementally so a
huge recording never needs a full download ([research-tooling / sdr-waterfall lenses](#sources);
[IQEngine](https://github.com/IQEngine/IQEngine)). Structurally it is **the same object as the
Confirmed/History catalogue entry** — a box in (time × frequency) with a label — which is what makes
it cheap to build: the canvas already draws exactly this geometry for detections.

### The object and routes

```
Annotation: { id, collection_id?, kind ("text"|"box"|"marker"),
              f_lo_hz, f_hi_hz, t0_s, t1_s,        // the box geometry (a text note is a zero-area point + label)
              label, body?,                         // short label + optional longer text
              author,                               // the token fingerprint — human-authored, always
              provenance, created_s, updated_s }

GET    /api/annotations                      ?f_lo=&f_hi=&t0=&t1=&limit=&cursor=   -> paged (below)
POST   /api/annotations                      { "kind", "f_lo_hz","f_hi_hz","t0_s","t1_s", "label", "body"?, "collection_id"? } -> Annotation (201)
GET    /api/annotations/{id}                 -> Annotation
PUT    /api/annotations/{id}                 any create field                       -> Annotation
DELETE /api/annotations/{id}                 -> { "deleted": Annotation }
```

`GET /api/annotations` is **paged/streamed, not whole-dataset**: it takes the same required
`f_lo`/`f_hi`/`t0`/`t1` box and `limit`/`cursor` paging as
[`/api/events`](api.md#get-apievents--the-durable-catalogue-of-events-t-264-adr-0017-tm-8), because a
long-lived research session can accumulate thousands of annotations and the canvas asks only about
its window. This is the IQEngine "stream annotations incrementally" property, expressed as the same
windowed-query contract every other durable surface in hackriff already uses. Clicking an annotation
in the table (§7) navigates the canvas to its box — *view arithmetic in time, and a retune only if
its frequency is outside the tuned window* (§6, §9).

### SigMF-adjacent export shape

Annotations export as SigMF `annotations` entries, cross-referencing
[docs/sigmf-extension.md](sigmf-extension.md). A hackriff annotation maps to a SigMF annotation object
with the standard `core:sample_start` / `core:sample_count` (derived from `t0_s`/`t1_s` against the
recording's sample rate and start), `core:freq_lower_edge` / `core:freq_upper_edge` (from
`f_lo_hz`/`f_hi_hz`), `core:label` (from `label`), `core:comment` (from `body`), plus a
`hackriff:annotation` extension block carrying the fields SigMF has no home for — `author`, the §2
`provenance` stamp, `collection_id`, and `authored: true`. The `authored: true` flag is load-bearing:
it is what distinguishes a **human-authored** annotation from the machine's `hackriff:truth`
ground-truth block (which tests assert against — [docs/sigmf-extension.md](sigmf-extension.md)) and
from a blind-detected emitter written to the same recording. An importer that reads a hackriff
recording sees authored annotations and machine truth as clearly different objects.

### How an annotation differs from a Confirmed emitter

They **share the box geometry** and both draw as time-frequency regions on the canvas, but they are
different in authority and origin, and the difference is the whole exploration-first invariant:

| | Confirmed emitter | Annotation |
|---|---|---|
| **Origin** | blind detection, then verification (CLAUDE.md) | a human drew it |
| **Authority** | a measurement of the air; carries a presence track | user metadata; carries no presence track |
| **Feeds detection?** | it *is* the detection record | **never** — see §9 |
| **Provenance** | detection provenance (gain, overload, spurs) | §2 authored provenance (`authored: true`) |
| **Box top** | runs to the live edge until an END is detected (T-410) | fixed at `t1_s`; a human-set extent does not grow |
| **Route family** | `/api/inventory`, `/api/events` | `/api/annotations` |

An annotation may *point at* a Confirmed emitter (a note like "this is the pager I was tracking"), but
it never becomes one, and a Confirmed emitter is never silently promoted from an annotation. The
machine finds; the human annotates; the surfaces are drawn in the same visual language so a
researcher reads them together, but the stores and the authority are separate.

---

## §6. Saved views (bookmarks of extent)

A **saved view** is a named, restorable (time × frequency) window a researcher jumps back to
instantly — ArcGIS's spatial bookmarks, applied to the (time × frequency) plane
([research-tooling lens](#sources);
[Esri bookmarks](https://pro.arcgis.com/en/pro-app/3.4/help/mapping/navigation/bookmarks.htm)).

```
SavedView: { id, name, note?,
             center_f_hz, span_f_hz,               // the frequency extent
             center_t_s?, span_t_s?, follow_live,   // the time extent; follow_live = pinned to the growing edge
             pane_layout?,                          // optional: the N-pane arrangement to restore
             provenance, created_s, updated_s }

GET/POST/GET/PUT/DELETE  /api/views[/{id}]          // same CRUD shape as /api/collections
```

**Argument: a saved view is just a named point in view-arithmetic state.** The canvas already reduces
the whole navigation surface to pure view arithmetic — a pane is
`(center_f, span_f, center_t, span_t)` plus a follow/frozen time window (docs/16 §8.4a). A saved view
is a *serialisation of that tuple*, nothing more. Restoring it is `applyTimeTarget` +
`applyFreqZoom` over already-captured data — the same client-side view arithmetic a scrub or a wheel
does ([docs/14, T-367](14-ui-rewrite.md)). This is why it is safe and instant: it moves no radio.

**How it differs from a device retune.** This is the same boundary the canvas already draws and must
not soften ([docs/14, T-343/T-392](14-ui-rewrite.md); the maps-philosophy "hard line at the device"
[principle](#sources)). Restoring a saved view whose frequency extent is **inside the currently tuned
window** is a pure view change — zero device calls. Restoring one whose frequency extent is **outside
the tuned window** cannot be satisfied by view arithmetic alone, because time is always a view over
already-captured data but a frequency outside the tuned window can only be reached by tuning there. So
a saved view restore that leaves the tuned band **offers a retune** through the one gated
`DeviceAction` path (snapped to the nearest achievable config, re-derived at commit, refused only if
no configuration can capture it), exactly as a pan to un-tuned spectrum does. A saved view is a
*bookmark of where you looked*, and looking somewhere else in frequency is the one navigation act that
commands the radio — the saved view does not get an exemption from that boundary; it inherits it.

Saved views are shareable and exportable like any collection object (§8): a named point in
view-arithmetic state serialises to a few numbers, so "send me the view where you found the anomaly"
is a link, not a screenshot.

---

## §7. The table is the second view of the map

Every mark — pin, box, measurement, annotation, saved view — is **also a row** in a sortable,
filterable, exportable table, and edits in either the table or the canvas propagate to the other. This
is the Felt model, and it is the single most important structural fix for "Google Maps can't build a
collection": *the table and the map are two views of one data* ([research-tooling lens](#sources);
[Felt](https://help.felt.com/getting-started/tour-the-interface)).

### The panel

A **Research** panel — a slide-in sidebar or the full state of the bottom sheet (docs/24) — with a
tab per durable object kind (Collections/Markers, Measurements, Annotations, Views) plus an "All"
tab. Each row shows the object's name/label, its place (frequency, and time when it has one), its
value (for measurements), its collection, and its provenance tier as a small badge. Rows are:

- **sortable** by frequency, time, value, collection, or authored-at;
- **filterable** by collection, kind, tier, and — when the researcher asks — by the current window
  (the same `f_lo`/`f_hi`/`t0`/`t1` box every windowed route takes);
- **exportable** (§8), per selection or whole.

The panel is durable-state only. It never lists ephemeral hover state, and it never lists blind
detections (those are the Explore Candidate/Confirmed lists and the History surface, which are the
machine's catalogue — [docs/14](14-ui-rewrite.md)). Keeping the researcher's marks in a *different*
panel from the machine's detections is the §1 visual-distinctness rule at the panel level.

### The sync contract with the canvas selection

The table and the canvas share **one selection state** and one collection of durable objects, held in
the client store, sourced from the durable routes. The contract, stated so a later edit cannot break
it (the "assert the request the client builds" guard — CLAUDE.md, T-367):

- **Selecting a row selects the mark**, and vice versa: clicking a table row highlights the object's
  box on the canvas and, if it is outside the window, offers to navigate there (§6 rules — a device
  action only if its frequency is un-tuned). Clicking a mark on the canvas scrolls the table to and
  selects its row.
- **Editing propagates through the backend, not around it.** Dragging a measurement cursor on the
  canvas, or editing a note in the table, is a `PUT` to the object's route; the store updates from the
  response; both surfaces re-render from the same store row. There is no client-side shortcut that
  updates one surface without the round-trip, because the value of a measurement is the backend's to
  compute (§4) and the audit trail is the backend's to write.
- **One store, two renderers.** The canvas renders the durable objects that fall in its window in the
  render pass, placed through the pane's own capture-time mapping like every other overlay (docs/16
  §8; [docs/14, T-362](14-ui-rewrite.md)); the table renders all of them as rows. Neither holds its
  own copy of the truth. This is the same "one filtered collection behind the list and the boxes"
  discipline the Explore inventory already keeps ([docs/14, T-386/T-389](14-ui-rewrite.md)).

---

## §8. Export and reuse

Export is **first-class, not an afterthought**: the research artifact must outlive the session. Felt
embeds and shares layers; QGIS/ArcGIS print composers preserve the exact legend and annotation state
that produced them ([research-tooling lens](#sources)). For hackriff:

- **Every durable object exports as a file or a link.** A collection (with its markers), a set of
  measurements, an annotation layer, or a saved view each has an export path. The natural container is
  **SigMF-adjacent**: annotations export as SigMF `annotations` (§5), markers and measurements as a
  `hackriff:research` sidecar JSON alongside the SigMF metadata, saved views as a small JSON of
  view-arithmetic state. A recording plus its research sidecar is a self-contained, portable research
  artifact — the IQEngine "the annotation travels with the recording" property, extended to the whole
  research register.
- **Import round-trips.** What exports imports: dropping a research sidecar or a SigMF file with
  authored annotations onto a session re-creates the collections, measurements, annotations and views,
  preserving ids where they do not collide and provenance always. This is what makes "send me the view
  where you found it" and "here is my collection of pager sites" actually work.
- **Offline-first.** Consistent with the product constraint (CLAUDE.md; one self-contained device):
  export writes locally and needs no connectivity; a share link or an upload to an external SigMF
  store syncs when online. Reference data (band plans, licences, TLEs) caches the same way. Nothing
  here assumes a network.
- **Nothing here is a source of truth for detection.** An exported and re-imported collection,
  measurement or annotation is **user metadata**. It is drawn, it is searchable, it is shareable — and
  it is never fed into blind detection on import, never pre-populates the inventory, and never
  overrides a measurement (§9; the exploration-first invariant, CLAUDE.md). Importing a colleague's
  annotations shows you where *they* looked; it does not tell your detector what is there.

---

## §9. Authoring gestures on one canvas

The canvas has one gesture vocabulary and must keep it. Since T-456/T-458 a **drag pans** and a
**wheel zooms** (a modifier gives one axis), and the T-412 lesson is that a second interpretation of a
drag is where drift and ambiguity enter. So authoring a mark cannot *also* be a bare drag.

**The resolution: authoring is a mode, entered explicitly, and it borrows the modifier discipline
already in place.** The canvas has a small tool selector (part of the floating chrome, docs/24):
**Navigate** (the default — drag pans, wheel zooms), **Measure** (drag lays down a measurement's
cursors), **Annotate** (drag draws an annotation box, click drops a text note), **Pin** (click drops
a marker into the active collection). In any authoring mode a drag creates a mark instead of panning;
the mode is visible (cursor + an active-tool indicator) so the user always knows which interpretation
a drag has. This is exactly the T-445 open capability #2 — *"a drag on the surface pans, so a
selection gesture needs a modifier or a mode that is not designed"* ([docs/14, T-445](14-ui-rewrite.md))
— now designed. One gesture vocabulary, one interpretation of a drag *at a time*, the mode being the
disambiguator the canvas already lacked.

This coordinates with, and does not replace, the existing selection path: selections are still created
from the capture band's time drag and drawn on the surface ([docs/14, T-445](14-ui-rewrite.md)); the
authoring modes add *durable-object* creation in the same content-space coordinates.

**Honesty rules — non-negotiable, and they are the exploration-first invariant applied to authoring:**

- **An authored annotation is user metadata and is never fed back into blind detection.** The detector
  runs on IQ and spectrum, blind, and its inputs are the air — never the annotation store. A note
  saying "pager here" changes no threshold, mints no candidate, and confirms no emitter. This is the
  same rule that keeps the band-plan database a *suggester* and never a source of truth (CLAUDE.md);
  an annotation is a *human's* suggestion and gets even less authority over detection than the
  database does — which is none.
- **Provenance records it as authored.** Every authored object carries `authored: true` and the §2
  stamp with `actor` set; on SigMF export it is a `hackriff:annotation` block distinct from
  `hackriff:truth`. A later reader — human or test — can always tell a human's mark from a machine's
  measurement, and a test fixture's hidden ground-truth list is never contaminated by a researcher's
  notes on the same recording.
- **Authoring is a view change, never a device action.** Creating any durable object carries no
  `device` key and reaches no radio route (§3, the device-action classification in
  [docs/api.md](api.md#device-actions-t-343)). The only research act that can command the radio is
  restoring or navigating to a saved view whose frequency is un-tuned (§6), and that goes through the
  same gated `DeviceAction` offer as every other retune — authoring the view did not move the radio;
  choosing to look there later might.

## §10. The store contract (normative)

*The contract MAP-16 (annotations), MAP-17 (collections + markers), MAP-18 (measurements) and MAP-19
(saved views) build to. §1–§9 argue it; this section is what a reviewer checks an implementation
against. Rationale: [ADR-0023](adr/0023-map-ui-and-research-state.md) §5. Each ticket updates
[`docs/api.md`](api.md) and `crates/hk-cli/tests/api_contract.rs` **together** (T-079); the shapes are
reserved in `docs/api.md` under "Reserved: the map-UI research routes" before any client exists.*

### §10.1 Four stores, one pattern

| Store | Objects | Route family | Ticket |
|---|---|---|---|
| Marker collections | `Collection`, `Marker` | `/api/collections`, `/api/collections/{id}/markers`, `/api/markers/{id}` | MAP-17 |
| Saved measurements | `Measurement` | `/api/measurements[/{id}]` | MAP-18 |
| Annotations | `Annotation` | `/api/annotations[/{id}]` | MAP-16 |
| Saved views | `SavedView` | `/api/views[/{id}]` | MAP-19 |

The object shapes are §3, §4, §5 and §6 above and are normative as written. MAP-16 lands the shared
pattern (paging, provenance, audit, error shape); MAP-17–19 are instances of it, which is why four
stores is one design and not four.

### §10.2 One provenance stamp, written by the backend

Every object in all four stores carries the §2 `provenance` block, **stamped by the server**, never
accepted from the client:

- The client sends only the **view context it was on** (`center_hz`, `span_hz`, `t_capture`, `tier`,
  and the pane's `device_id` where it has one). The server records what that means, adds `actor` (a
  token fingerprint — **never the token**), `authored_s`, and `authored: true`.
- **`t_capture` is on the capture clock; `authored_s` is on the wall clock; they are never
  conflated.** This is the bug the UI has found five times (T-379/T-384/T-389 and the inspector); a
  browser instant compared against capture time is wrong by the replay's offset. A measurement refers
  to *when the air was*; the audit trail records *when the human acted*.
- A request whose `provenance` block contains a server-owned field (`actor`, `authored_s`,
  `authored`) is **`400 invalid`**. Provenance is evidence, not input.

### §10.3 One paging contract — "durable" is not "unbounded"

Every list route takes the `/api/events` paging contract: `limit`, `cursor`; answers carry `count`,
`matched` and `next_cursor`. Every list route also accepts the optional window box
`f_lo`/`f_hi`/`t0`/`t1`; **`GET /api/annotations` requires it**, because a long research session
accumulates annotations the way the catalogue accumulates events.

| Route | `limit` default | max |
|---|---|---|
| `GET /api/annotations` | 200 | 2000 |
| `GET /api/collections`, `…/markers`, `/api/measurements`, `/api/views` | 500 | 2000 |

A list route that could grow without limit is a defect whichever store it belongs to.

### §10.4 `POST /api/measurements` carries cursors, never a value

The thin-client rule, made enforceable by the contract instead of by review:

- The body carries `kind`, `cursors` (the place), optional `n`, `note`, `collection_id`. A body
  containing **`value` or `unit` is `400 invalid`**.
- The backend computes `value`/`unit` **from the data under that place** and stamps provenance. A −3 dB
  bandwidth is a function of the noise floor and the actual −3 dB points; a symbol rate is a function
  of the signal. Neither is a function of where a cursor landed, so neither may be computed in
  `ui/src`.
- The client's **live drag readout is ephemeral presentation arithmetic** over its own pixel↔(Hz, s)
  maps — the same class it already does for the hover readout and the retune snap. *Saving* is what
  crosses into an object.
- A `PUT` that moves a cursor re-computes the value server-side. There is no path by which a stored
  value is something the client computed and the backend merely filed.

### §10.5 Audit, availability, and the device line

- Every mutating request (`POST`/`PUT`/`DELETE`) on all four stores is **audited** exactly as
  `/api/bookmarks*` and `/api/selections*` are: token id (never the token), peer, action, body,
  old/new, status.
- **No audit log ⇒ every mutating endpoint answers `503 unavailable`**, consistent with the rest of
  the control API.
- **No entry ever carries a `device` key.** Authoring a durable object is a view act and reaches no
  radio (§9). The only research act that may command the radio is *restoring* a saved view whose
  frequency extent lies outside the tuned window, which inherits the ordinary gated `DeviceAction`
  offer and gets no exemption.
- Error shape follows the control API: `400 invalid`, `404 not_found`, `409 conflict` (a supplied id
  that already exists), `503 unavailable` (no store / no audit log), `405` with `Allow` for a wrong
  method, `401` before dispatch for a missing or wrong token.

### §10.6 `/api/bookmarks` becomes a facade; `/api/views` is its own store

- **Bookmarks.** On first start with a collection store, existing bookmarks migrate into a reserved,
  un-deletable collection (`"Bookmarks"`), each as a frequency-only marker (`t_center_s = null`),
  preserving id, name, note and timestamps. `/api/bookmarks*` stays live as a **compatibility facade**
  over that one collection — same shapes, same audit action names. New clients use `/api/collections`;
  the two see the same rows. Two stores for one idea is the drift the canvas cutover existed to kill.
- **Saved views are not markers.** A view is a serialised point in view-arithmetic state —
  `(center_f, span_f)`, an optional `(center_t, span_t)`, `follow_live`, an optional pane layout — and
  has no frequency *centre* in the sense a marker does. Filing it as a marker would give it a false
  place. It gets its own small store, `/api/views`, on the same pattern. (This settles the option
  MAP-19 left open.)

### §10.7 Nothing here feeds blind detection

An authored mark mints no candidate, moves no threshold, confirms no emitter, sets no family and
never pre-populates the inventory — **on create or on import**. Detection's inputs are the air. On
SigMF export an annotation is a `hackriff:annotation` block with `authored: true`, structurally
distinct from the `hackriff:truth` ground-truth block a blind acceptance test asserts against, so a
researcher's notes on a fixture can never contaminate its hidden truth list.

---

## Proposed tickets and time estimate

> **Superseded as a plan by [`docs/26`](26-map-ui-redesign-tickets.md)** (the approved MAP-00…MAP-25
> set, in `docs/tasks.yaml` as T-800…T-825): R-1 → MAP-17, R-2 → MAP-18, R-3 → MAP-16, R-4 → MAP-19,
> R-5 → folded into all four as the shared §10.2 stamp, R-6 → MAP-20/MAP-22, R-7 → MAP-21,
> R-8 → MAP-23. Kept for the reasoning behind the sequencing.

The user asked for a proposed ticket set and a time estimate. These slot into the MCANVAS
follow-up family (T-457–T-459, T-470–T-475) and depend on the chrome/layers/pins work in docs/24; the
coordinator finalises ids, deps and `parallel_group`. Estimates are engineer-days for one developer
at this repo's cadence, and assume the docs/24 chrome reframe lands first or in parallel.

| Proposed | Scope | Model / effort | Est. |
|---|---|---|---|
| **R-1** | Collection + marker store and `/api/collections`, `/api/markers` routes; bookmark migration + `/api/bookmarks` facade; contract tests (T-079) and audit. `core_interface`. | Opus, high | 3–4 d |
| **R-2** | Saved-measurement store and `/api/measurements`; backend Δf/Δt/bandwidth/duration/symbol-rate computation over the data (RESEARCH-003); contract tests. `core_interface` (measurement is a data measurement, not presentation). | Fable/Opus, high | 4–5 d |
| **R-3** | Annotation store, `/api/annotations` (paged), and the SigMF export/import mapping (§5) cross-referenced to docs/sigmf-extension.md. | Opus, high | 3–4 d |
| **R-4** | Saved-views store and `/api/views`; restore-as-view-arithmetic with the un-tuned-frequency retune-offer boundary (§6). | Sonnet (over R-1's pattern), Opus reviews the device boundary | 2 d |
| **R-5** | Provenance stamp (§2): the shared authored-provenance object written by all four stores; capture-clock vs wall-clock discipline; one contract test asserting the split. `core_interface`. | Opus, high | 2 d |
| **R-6** | Client: the four authoring modes (§9) over the T-458 gesture layer; mode indicator; one-interpretation-of-a-drag guard test (assert the request the client builds). Thin client. | Opus (waterfall/gesture-adjacent) | 3–4 d |
| **R-7** | Client: the Research table panel (§7) with two-way canvas↔table sync; per-object render on the canvas in the render pass. Thin client. | Sonnet, Opus reviews sync contract | 3–4 d |
| **R-8** | Export/import paths and the offline-first sidecar (§8); round-trip test. | Sonnet | 2 d |

**Total: ~22–27 engineer-days (~5–6 weeks)** for the research-tooling half, on top of the docs/24
chrome/layers/pins work, which is a separate estimate in that document. R-1, R-2, R-3 and R-5 are the
backend spine and can partly parallelise once R-5's provenance shape is fixed; R-6, R-7 and R-8 are
the thin-client surfaces and follow their routes. *These estimates are unverified — first-pass
sizing against comparable route+store+contract-test tickets in this repo, not a measured cycle-time.*

---

## Sources

Research synthesised for this design (the five lenses referenced inline):

**Maps philosophy / chrome and direct manipulation**
- Direct Manipulation (NN/g) — https://www.nngroup.com/articles/direct-manipulation/
- Shneiderman, *Direct Manipulation for Comprehensible, Predictable and Controllable UIs* (1997, PDF) — https://www.cs.umd.edu/~ben/papers/Shneiderman1997Direct.pdf
- Progressive Disclosure (IxDF) — https://ixdf.org/literature/topics/progressive-disclosure
- Bottom sheets — Material Design 3 — https://m3.material.io/components/bottom-sheets/overview
- Layers | Maps JavaScript API (Google) — https://developers.google.com/maps/documentation/javascript/layers
- Data Layer | Maps JavaScript API (Google) — https://developers.google.com/maps/documentation/javascript/datalayer
- Immersive content (Android Developers) — https://developer.android.com/design/ui/mobile/guides/layout-and-content/immersive-content
- display (Web app manifest, MDN) — https://developer.mozilla.org/en-US/docs/Web/Progressive_web_apps/Manifest/Reference/display

**Data-projection surface / layers, sources, pins, clustering**
- Mapbox GL JS: create and style clusters — https://docs.mapbox.com/mapbox-gl-js/example/cluster/
- deck.gl documentation — https://deck.gl/docs
- kepler.gl — Filters — https://docs.kepler.gl/docs/user-guides/e-filters
- keplergl/kepler.gl (GitHub) — https://github.com/keplergl/kepler.gl
- Marker — Map UI Patterns — https://mapuipatterns.com/marker/
- Google Maps: make markers clickable and accessible — https://developers.google.com/maps/documentation/javascript/advanced-markers/accessible-markers
- Google Maps Help: use layers — https://support.google.com/maps/answer/3092439

**GIS-UX / cartography / figure-ground / accessibility**
- Principles of Map Design in Cartography (Esri) — https://www.esri.com/arcgis-blog/products/arcgis-pro/mapping/design-principles-for-cartography
- Graphic design principles for mapping: Figure-ground (Esri) — https://www.esri.com/arcgis-blog/products/product/mapping/graphic-design-principles-for-mapping-figure-ground-organization
- Guide to map design (Mapbox) — https://www.mapbox.com/insights/map-design-process
- Semantic Zoom — https://www.emergentmind.com/topics/semantic-zoom
- Designing Maps for Colorblind Readability (Esri) — https://www.esri.com/arcgis-blog/products/arcgis-pro/mapping/designing-maps-for-colorblind-readability
- Controls | Maps JavaScript API (Google) — https://developers.google.com/maps/documentation/javascript/controls

**Research tooling / durable state / provenance**
- Tour the interface (Felt Help Center) — https://help.felt.com/getting-started/tour-the-interface
- Editing layers (Felt Help Center) — https://help.felt.com/layers/editing-layers
- kepler.gl documentation — https://docs.kepler.gl/
- Measure — ArcGIS Pro Documentation — https://pro.arcgis.com/en/pro-app/3.4/help/mapping/navigation/measure.htm
- IQEngine (GitHub) — https://github.com/IQEngine/IQEngine
- Observable: from data exploration to data apps — https://observablehq.com/blog/from-data-exploration-to-data-apps-with-observable

**SDR waterfall precedents / annotation-as-data**
- Maia SDR — waterfall rendering architecture — https://maia-sdr.org/about/
- OpenWebRX wiki: how the bookmarks work — https://github.com/jketterl/openwebrx/wiki/How-the-bookmarks-work
- SDRangel spectrum markers — https://github.com/f4exb/sdrangel/blob/master/sdrgui/gui/spectrummarkers.md
- Raven Pro — Cornell Lab of Ornithology — https://www.ravensoundsoftware.com/software/raven-pro/
- SPACE: SPectrogram Analysis and Cataloguing Environment (arXiv) — https://arxiv.org/pdf/2207.12454
- Bottom Sheets: Definition and UX Guidelines (NN/g) — https://www.nngroup.com/articles/bottom-sheet/

**Internal (this repo)**
- [docs/14 — UI rewrite / MUI](14-ui-rewrite.md); [docs/16 §8 — the unified surface (MCANVAS)](16-coverage-tile-pyramid.md).
- [docs/api.md](api.md) — `/api/bookmarks`, `/api/selections`, `/api/inventory`, `/api/events`, `/api/coverage`, `/api/tiles`, `/api/tiles/events`, device-action classification and audit.
- [docs/sigmf-extension.md](sigmf-extension.md) — the `hackriff` SigMF namespace and provenance block.
- CLAUDE.md — the signal & inventory model, the view/canvas invariants, and the exploration-first rule; [ADR-0017](adr/0017-time-extent-signal-model.md), [ADR-0019](adr/0019-presence-as-an-interval-with-endpoints.md).
