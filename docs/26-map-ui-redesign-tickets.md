# Map-UI redesign — proposed tickets

Proposal from the map-ui-research workflow (2026-09-21), **approved by the user 2026-09-22 and in `docs/tasks.yaml` as T-800…T-825** (MAP-`nn` = T-8`nn`), milestone **MMAP**. Thin-client rule holds: most are `ui/` presentation; `layer=api|both` marks the few needing backend/API additions. The design contract is [docs/23](23-map-ui-philosophy.md) (§10–§11 normative), [docs/24](24-canvas-as-data-surface.md) (§13–§15 normative) and [docs/25](25-spectrum-research-workflow.md) (§10 normative), decided in [ADR-0023](adr/0023-map-ui-and-research-state.md); the layout reference is [`ui/mockups/map-ui-v1.html`](../ui/mockups/map-ui-v1.html).

## Tickets

| # | Title | Size | Layer | Deps | Use-cases |
|---|---|---|---|---|---|
| MAP-00 | Design + ADR-0023: map-UI chrome, the layer model, and the research-state stores | L | both | — | AWARE-042, AWARE-053, RESEARCH-003 |
| MAP-01 | Full-bleed layout: canvas is 100vw x 100vh, the app-shell frame retires | M | ui | MAP-00 | — |
| MAP-02 | Floating control cluster: Go-to, layers button, live/my-location FAB, zoom — semi-transparent, edge-docked, fadeable | M | ui | MAP-01 | — |
| MAP-03 | Bottom-sheet component: non-modal, draggable peek/half/full snap states | M | ui | MAP-01 | — |
| MAP-04 | Detail sheet on selection: clicking a pin/box opens the focused signal | M | ui | MAP-03 | AWARE-053 |
| MAP-05 | HUD axes: floating frequency (bottom) and time (left) rulers with ticks+labels, anchored in content space | M | ui | MAP-01 | AWARE-042 |
| MAP-06 | Per-pane layer registry + layers menu (two axes: base style vs overlays) | L | ui | MAP-00, MAP-02 | AWARE-042 |
| MAP-07 | Coverage-fog layer: grey/unknown/unobserved/excluded as an explicit toggleable figure-ground plane | S | ui | MAP-06 | AWARE-042 |
| MAP-08 | Detections/events layer: confirmed prominent, candidate weaker, never stacked | S | ui | MAP-06 | AWARE-053 |
| MAP-09 | Pins: signal/event markers with rest/hover(MapTip)/selected states | L | ui | MAP-06, MAP-04 | AWARE-053 |
| MAP-10 | Pin clustering at coarse zoom from /api/tiles/events, resolving to individuals on zoom-in | M | ui | MAP-09 | AWARE-042 |
| MAP-11 | Artifacts layer: image/harmonic/IMD relationships drawn as linked overlays to their source | M | ui | MAP-06 | AWARE-042 |
| MAP-12 | Band-plan priors layer + GET /api/priors viewport route (suggestions, never truth) | M | both | MAP-06 | AWARE-042 |
| MAP-13 | Figure-ground + colorblind-safe encoding pass across all layers | S | ui | MAP-07, MAP-08, MAP-09 | — |
| MAP-14 | Explore drawer: a collapsed bottom sheet of interesting signals and past surveys | M | ui | MAP-03 | AWARE-042 |
| MAP-15 | Past-surveys browse + jump-to-a-saved-coverage-window | M | ui | MAP-14, MAP-19 | AWARE-042 |
| MAP-16 | Annotation store + /api/annotations (durable, SigMF-compatible, provenance) | L | api | MAP-00 | RESEARCH-003, RESEARCH-001 |
| MAP-17 | Marker collections: generalize /api/bookmarks to time-frequency markers grouped into named collections | M | api | MAP-00 | RESEARCH-003 |
| MAP-18 | Saved measurements store + /api/measurements (value+unit+place+time+provenance) | M | api | MAP-00 | RESEARCH-003 |
| MAP-19 | Saved views: named, restorable (time x frequency) window extents | S | api | MAP-00 | AWARE-042 |
| MAP-20 | Annotation authoring on the canvas (draw box/marker/label -> create annotation) | L | ui | MAP-16, MAP-04 | RESEARCH-003, RESEARCH-001 |
| MAP-21 | Collections panel + table view synced with the canvas (every mark is also a row) | L | ui | MAP-17, MAP-06 | RESEARCH-003 |
| MAP-22 | On-canvas measurement tool: drag to measure Delta-f/Delta-t/bandwidth/duration, persist it | M | ui | MAP-18 | RESEARCH-003 |
| MAP-23 | Export: collections, measurements, annotations out as file/link (SigMF-adjacent) | M | both | MAP-21, MAP-22 | RESEARCH-003 |
| MAP-24 | Phone-width + fade/immersive pass across the floating chrome | M | ui | MAP-05, MAP-14, MAP-21 | — |
| MAP-25 | UI tests + honesty guards for the map-UI and research surfaces | M | ui | MAP-09, MAP-10, MAP-12, MAP-20, MAP-21, MAP-22 | — |

## Detail

### MAP-00 — Design + ADR-0023: map-UI chrome, the layer model, and the research-state stores  (L, both)
Opus, core_interface. The blocking design task. Produce ADR-0023 and the three docs (23/24/25) as specs: the full-bleed layout + overlay taxonomy, the per-pane LAYER registry and paint order over the one WebGL2 context, the PIN model (states/clustering/picking), and the four durable research stores (collections, measurements, annotations, saved views) with their route shapes and provenance stamp. Define the client state-store extensions and the frontend<->API mapping for every new panel. Must EXTEND docs/16 §8 + docs/14, contradict no invariant, and keep the thin-client rule. Justify no new heavyweight FE deps (stay vanilla TS + the existing reactive store).

### MAP-01 — Full-bleed layout: canvas is 100vw x 100vh, the app-shell frame retires  (M, ui)
Reframe the Explore surface to Google-Maps geometry. The one WebGL2 canvas fills the viewport; the app-shell sidebars/focus/dock stop framing it and become floating overlays (wired in later tickets). Chrome floats in SCREEN space; data overlays stay in CONTENT space and re-lay-out every frame (unchanged MCANVAS rule). Responsive scaffold to phone width, no horizontal page scroll. FE only.

### MAP-02 — Floating control cluster: Go-to, layers button, live/my-location FAB, zoom — semi-transparent, edge-docked, fadeable  (M, ui)
The primary floating controls as z-axis overlays docked to viewport edges: a Go-to-frequency/search input, the layers-menu button, a live 'my-location' FAB that snaps a pane to the growing edge, and zoom affordances. Semi-transparent, fade when idle. Go-to and the FAB build a DeviceAction only on explicit press (the view/device line); pan/zoom never command the radio. FE only.

### MAP-03 — Bottom-sheet component: non-modal, draggable peek/half/full snap states  (M, ui)
A reusable bottom sheet that coexists with the still-live, still-pannable canvas (Material/NNg pattern): three snap states, content reflows per state, never modals over the surface. Hosts the detail sheet (MAP-04) and the Explore drawer (MAP-14). Per-viewer collapse state in localStorage (try/catch, render-correct without it). FE only.

### MAP-04 — Detail sheet on selection: clicking a pin/box opens the focused signal  (M, ui)
Rehome MUI's focus panel into the bottom-sheet/slide-in idiom. On selecting a pin/box: big frequency, measurements with their measured-at time (the /api/inventory `measured` object), ranked explanations (suggestions never truth), liveness, and actions (Listen, Decode/RDS, Record clip, Stream out, Promote/Delete, Analyze). Reads existing routes only. FE only.

### MAP-05 — HUD axes: floating frequency (bottom) and time (left) rulers with ticks+labels, anchored in content space  (M, ui)
Finish the open T-459 inside the new layout: real tick rulers (not just the chrome readout line) for both axes, anchored in capture-time/frequency so they move with pan/zoom and re-lay-out every frame, fading with the chrome. Ticks snap to the pane's drawn level; labels state units. Supersedes/absorbs T-459. FE only.

### MAP-06 — Per-pane layer registry + layers menu (two axes: base style vs overlays)  (L, ui)
The projection-surface core (docs/24). A per-pane registry of toggleable layers with explicit z-order/paint order over the one context; a Maps-style layers menu with two independent axes (base ramp/phosphor style vs overlay content), per pane. Each layer is a pure (data, accessor) render. Overlay quads stay strokes/marks (no sampler, no ramp) so a layer can never tint a measurement — keep the byte-identical-with-overlays-on/off guard. FE only.

### MAP-07 — Coverage-fog layer: grey/unknown/unobserved/excluded as an explicit toggleable figure-ground plane  (S, ui)
Make coverage an explicit toggleable layer with figure-ground styling (base recedes; grey=genuinely unobserved stays honest; the four states drawn distinctly per resolution.grey_rule). Reads GET /api/coverage and the tile coverage plane — no new data. FE only.

### MAP-08 — Detections/events layer: confirmed prominent, candidate weaker, never stacked  (S, ui)
Confirmed (prominent) and candidate (weaker) detections as a layer of time-frequency boxes/events from GET /api/events, honoring the overlap-is-an-error invariant (never hide/stack/z-order overlaps — an overlap reaching the screen is a backend bug to report). Unknowns not hidden by default. FE only.

### MAP-09 — Pins: signal/event markers with rest/hover(MapTip)/selected states  (L, ui)
First-class interactive markers placed in capture-time/frequency (moving with pan/zoom): rest glyph, hover MapTip (centre/bandwidth/family-suggestion/on-air), selected -> opens the detail sheet. Keyboard/hover/click accessible. GPU/quadtree picking sized for dense point sets. Distinguish auto detection markers from human collection markers (MAP-21). FE only.

### MAP-10 — Pin clustering at coarse zoom from /api/tiles/events, resolving to individuals on zoom-in  (M, ui)
At coarse zoom pins cluster from the existing GET /api/tiles/events count-per-cell aggregate and expand into individuals on zoom-in, mirroring the tile pyramid's coarsen/refine. Enforce the coarse-zoom honesty rule: a sub-pixel burst is a minimum-size marker or a count, NEVER an inflated box. FE only.

### MAP-11 — Artifacts layer: image/harmonic/IMD relationships drawn as linked overlays to their source  (M, ui)
Draw artifact-of relationships (image/harmonic/intermod) from /api/inventory relate claims as LINKED overlays that visually tie a spur/image to the source emitter — the signal-relationships goal, geometric tier. Ranked evidence with reasoning, never truth. FE only (reads existing inventory relate fields).

### MAP-12 — Band-plan priors layer + GET /api/priors viewport route (suggestions, never truth)  (M, both)
Both. Backend: a read-only GET /api/priors?f_lo&f_hi&t0&t1 returning ranked band-plan/licence allocations as EXPLANATIONS for the viewport, gated identical to /api/events, computed on demand, never a tile channel, never pre-populating the inventory. Update docs/api.md + api_contract.rs together (T-079). FE: a toggleable priors layer drawn as labeled allocation bands with ranked reasoning — the exploration-first DB-suggests invariant rendered. Small backend surface, larger FE.

### MAP-13 — Figure-ground + colorblind-safe encoding pass across all layers  (S, ui)
Apply docs/23 §7 / docs/24 §8: base desaturated and receding, small controlled symbol vocabulary, collision-aware labels, and state (candidate/confirmed/unknown, observed/unobserved) encoded with shape/pattern as well as hue (CVD-safe; no red-green-only). Verify against the honesty-tier distinctness. FE only.

### MAP-14 — Explore drawer: a collapsed bottom sheet of interesting signals and past surveys  (M, ui)
A collapsed-by-default bottom sheet (peek->half->full) surfacing places to go: quiet-but-active bands, recent anomalies, strongest current signals, and past-survey windows. Reads GET /api/scheduler (POI), /api/events, /api/coverage, /api/analysis/strongest — all existing. Coexists with the live surface; selecting an item pans/zooms (or offers a retune). FE only.

### MAP-15 — Past-surveys browse + jump-to-a-saved-coverage-window  (M, ui)
In the Explore drawer, list prior survey coverage windows (from the coverage map / observation log via existing routes) and jump the pane to one, restoring the (time x frequency) extent. Distinguish observed-then vs never-looked. FE only.

### MAP-16 — Annotation store + /api/annotations (durable, SigMF-compatible, provenance)  (L, api)
Backend. A durable annotation store: time-frequency notes (text or graphical box/marker) with provenance (device, centre/span, capture window, tier), clickable-to-navigate, paged. GET/POST/PUT/DELETE /api/annotations, audited + 503-unavailable like the rest of the control API, SigMF-adjacent export shape (align docs/sigmf-extension.md). Update docs/api.md + api_contract.rs together (T-079). Never fed back into blind detection.

### MAP-17 — Marker collections: generalize /api/bookmarks to time-frequency markers grouped into named collections  (M, api)
Backend. Extend the existing frequency-only bookmark store into TIME-FREQUENCY markers grouped into named, toggleable COLLECTIONS: GET/POST/PUT/DELETE /api/collections and members, backward-compatible with existing bookmarks, audited. Update docs/api.md + api_contract.rs together (T-079).

### MAP-18 — Saved measurements store + /api/measurements (value+unit+place+time+provenance)  (M, api)
Backend. Persist a measurement as an object: value + unit + (f,t) place + time + provenance — Delta-f, Delta-t, bandwidth, duration, symbol-rate/period. GET/POST/PUT/DELETE /api/measurements, audited. The measurement itself is client-computed presentation arithmetic over already-known state (thin-client); the store keeps the durable object. Update docs/api.md + api_contract.rs together (T-079).

### MAP-19 — Saved views: named, restorable (time x frequency) window extents  (S, api)
Backend. Named restorable view extents (ArcGIS-bookmark analogue): a saved view is a point in view-arithmetic state, distinct from a device retune. **Settled by MAP-00: its own small `/api/views` store, not an extension of the bookmark/marker store** — a view has no frequency *centre* in the sense a marker does, so filing it as one would give it a false place (docs/25 §10.6). Same pattern as the other three stores; shareable/exportable. Update docs/api.md + api_contract.rs together (T-079).

### MAP-20 — Annotation authoring on the canvas (draw box/marker/label -> create annotation)  (L, ui)
FE. The gesture to author an annotation on the surface (box, marker, or label). **Settled by MAP-00 (docs/23 §10.4):** an explicit, visible **tool mode** re-binds only the *bare* drag; `Shift + drag` keeps its one meaning (mark a region) in every mode, so authoring adds no gesture to the vocabulary. Posts to /api/annotations with a provenance stamp; drawn as a distinct human-authored overlay. Depends on T-458 landing the selection-gesture design.

### MAP-21 — Collections panel + table view synced with the canvas (every mark is also a row)  (L, ui)
FE. The Felt model: a collection is a named, toggleable-as-a-layer set of markers/annotations, and every mark is ALSO a sortable/filterable ROW in a table; edits in either surface propagate. Toggle a collection on/off as an overlay layer (MAP-06). Reads/writes /api/collections + /api/annotations.

### MAP-22 — On-canvas measurement tool: drag to measure Delta-f/Delta-t/bandwidth/duration, persist it  (M, ui)
FE. A measurement mode: drag/cursors to read Delta-f, Delta-t, bandwidth, duration and symbol-rate/period (inspectrum-style, RESEARCH-003); the readout stays ON the canvas and can be SAVED to /api/measurements as a durable object (not a vanishing tooltip). Coordinates with T-458's gesture design. Pure presentation arithmetic over known view state.

### MAP-23 — Export: collections, measurements, annotations out as file/link (SigMF-adjacent)  (M, both)
FE + thin API glue. First-class export path for each durable object type so the research artifact outlives the session; SigMF-adjacent for annotations. Offline-first (export when online). No new signal logic.

### MAP-24 — Phone-width + fade/immersive pass across the floating chrome  (M, ui)
FE. Responsive to ~400px with no horizontal scroll; chrome fades when idle and returns on interaction; sheets and menus reachable one-handed. Verify the view/device line survives touch (pinch-zoom = view, region-select = retune offer). FE only.

### MAP-25 — UI tests + honesty guards for the map-UI and research surfaces  (M, ui)
FE. Request-shape assertions (assert the request the client BUILDS, not just what it renders — the T-367 gap), byte-identical-data-with-overlays-on/off, no-fabricated-timespan at coarse zoom (pins/counts not boxes), spy-client empty-call-list after the full gesture+layer vocabulary (pan/zoom/layer-toggle never command the radio), and the table<->canvas sync property. Extend ui/test + e2e.

## Time estimate

SIZING (ideal engineer-days, before parallelism/gate overhead): S=0.5d, M=1d, L=2d.\n\nBy group: Design MAP-00 (L) = 2d. Chrome MAP-01..05 (5 x M) = 5d. Layers MAP-06 (L) + 07 (S) + 08 (S) + 09 (L) + 10 (M) + 11 (M) + 12 (M) + 13 (S) = 2+0.5+0.5+2+1+1+1+0.5 = 8.5d. Explore MAP-14 (M) + 15 (M) = 2d. Research-API MAP-16 (L) + 17 (M) + 18 (M) + 19 (S) = 2+1+1+0.5 = 4.5d. Research-FE MAP-20 (L) + 21 (L) + 22 (M) + 23 (M) = 2+2+1+1 = 6d. Polish MAP-24 (M) + 25 (M) = 2d.\n\nTOTAL: 26 tickets, ~30 ideal engineer-days of work (2 L-heavy design/store tickets, 4 other L, 13 M, 4 S). Breakdown: 6 L (12d) + 13 M (13d) + 4 S (2d) + design already counted; = ~30d.\n\nBackend vs frontend: only 5 tickets touch the backend/API (MAP-12 partial, MAP-16, MAP-17, MAP-18, MAP-19) totaling ~6.5d; the design ADR (MAP-00) is both. The remaining ~21 tickets are thin-client presentation over docs/api.md, which is the intended shape (the canvas already exists; this is chrome + layers + research surfaces).\n\nCRITICAL PATH (longest dependency chain, not total work): MAP-00 (2d) -> MAP-06 (2d) -> MAP-09 (2d) -> MAP-20/MAP-21 (2d) -> MAP-23/MAP-25 (1d) ~= 6 sequential ticket-depths, ~9 ideal-days if run strictly serially on the spine. The four API stores (MAP-16..19) parallelize behind MAP-00, and the chrome tier (MAP-01..05) parallelizes with the layer tier once MAP-01 lands.\n\nCALENDAR (one developer + autonomous multi-agent landing several tickets/day): the binding constraint is NOT raw work but (a) dependency depth ~6-8 merge cycles on the spine, and (b) the observed cycle time — per docs/iteration-speed-analysis and cycle-time telemetry, the gate is ~8% of a ticket's cycle while queue wait (median ~120 min) and commit->merge (median ~270 min) dominate, so effective throughput is roughly 3-5 merged tickets/day even with parallel agents. 26 tickets at ~4/day throughput = ~6.5 working days of throughput, but the ~7-layer dependency spine cannot be compressed below ~7 merge cycles. Net: ~8-12 working days, i.e. ~2 to 2.5 calendar weeks, with MAP-00 (blocking) front-loaded on day 1.\n\nASSUMPTIONS & RISKS: (1) MAP-00 is Opus/core_interface and must land before fan-out — a slow or contested design review shifts everything right. (2) MAP-20 and MAP-22 depend on T-458 (open MCANVAS follow-up) settling the create-vs-pan gesture; if T-458 is not done first, add ~1-2d and a coordination round. (3) The four durable stores (MAP-16..19) assume the existing bookmark/selection store patterns generalize cleanly; a schema/migration surprise adds ~1d each. (4) Estimate excludes hardware, MAUTO content-tier signal work (MAP-11/artifacts is geometric only), and any renegotiation of an ACCEPTED ADR. (5) Full-workspace gate runs on every crate-touching merge (5 API tickets), so those 5 pay the ~21-min gate median each; the ~21 UI-only tickets take the cheaper test-ui path. (6) Numbers assume the current ~4 building-agents cap and the serial merge-runner; more agents raise throughput but not the dependency-depth floor.

