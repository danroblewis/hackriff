# MUI — Exploratory UI rewrite

Status: **brief for the coordinator** (2026-09-15, from the user via the supervisor). This is a new milestone. It was mocked up and deferred repeatedly but never planned; do it now. Read this, add MUI to `docs/11-roadmap.md`, add MUI tasks to `docs/tasks.yaml` with parallel_groups, and fan out. Runs in parallel with the tail of M2. Commit per task.

## Why

The current web UI is a thin client that has had M0b/M1/M2 panels **appended to the bottom** as they were built, so it stacks into a long scroll and doesn't match the product's exploratory intent. The user finds it hard to use. The backend is API-first (`docs/api.md` + contract tests from T-079), so the frontend can be replaced without touching the backend.

## The target

A **one-screen, exploratory UI** that fits without full-page scrolling and works down to phone width. The concrete design target is the mockup in the repo: **`ui/mockups/explorer-v3.html`** (open it — it is the spec, on example data). Build the real thing against the live API to match its layout and interactions.

Two modes via a top-bar toggle: **Explore** and **Decode**, an **Outputs dock** across the bottom visible in both, and an **always-on Capture timeline**.

### Explore
- **Left sidebar:** Signal inventory split into **Candidates** and **Confirmed** (per T-078), sortable, with **Promote / Delete** reachable without horizontal scroll; **Selections** below with their actions reachable. (Fixes the current sidebar button-overflow.)
- **Centre:** spectrum trace + WebGL waterfall; brackets mark confirmed/candidate/focused signals; DC masked; drag to select a region; hover readout.
- **Right focus panel:** the selected signal — big frequency, refined-from-output note, measurements, **ranked explanations** (suggestions, never truth), and actions (Listen, Decode/RDS, Record→Export clip, Stream out, Promote/Delete).

### Decode workbench
- **Pipelines** list (parallel decoders, incl. hop-following), the selected recipe's **stages**, and a blocks palette.
- **Per-stage plots** (MPX/subcarrier, eye, timing, constellation, sync search, group/frame counts, channel map).
- **Packet inspector** (the M1 inspector, given its proper home): frame list → hex+ASCII → layer tree, with **linked selection** both ways; shows the served byte-record and its TCP address.
- Stage **parameters** with blind-estimate "Use" suggestions and live quality (CRC/BCH pass rate, records/s, lock).

### Outputs dock
Every live stream (audio + decoded records) with meter/rate, mute, copy-address, stop. Listen **adds** a stream; a selection can Listen-to-all.

### Capture timeline
Always-on recording (no Record button); a scrubbable timeline; "reviewing N ago / LIVE".

### Added scope from docs/15 §7 (2026-09-15, user)
These are general UI features the user wants whether or not auto-decode exists. They build on the delivered MUI (T-149..T-179). MAUTO itself stays unscheduled.
- **Confirmed-signal boxes.** Every Confirmed signal is drawn as a **yellow box** that spans **both** the spectrum trace and the waterfall, not just a waterfall overlay. The box has **draggable left/right edges**. Dragging sets a user-adjusted band on the emitter through the API. The measured band is kept alongside it with provenance; it is never overwritten.
- **Right-click context menu.** Right-clicking a signal, box or inventory row opens a menu with the actions now in the right focus bar: Listen, Decode, **Analyze / synthesize decoder**, Record / Export clip, Stream out, Promote, Delete, Adjust band. The right panel is then **freed for display**; the focus panel keeps measurements and explanations.
- **Manual multi-band region select.** Drag on the waterfall to add several bands at once. Drag on the **history / capture timeline** to select a past time window: a selection with `t_lo`/`t_hi`, analysable from the IQ ring. Both extend the existing selections API (T-052), which already carries `t_lo`/`t_hi`.
- **Per-signal output panels** in the freed space. Digital signals get the **packet inspector** (frame list → hex+ASCII → field tree). **FM/AM** signals get a **waveform / audio scope** plus **RDS text** for FM. Several confirmed signals can each own a panel (T-071 concurrent demod).
- **Analyze action (stub now).** "Analyze / synthesize decoder" on a selection or signal calls `POST /api/analyze`. Until MAUTO lands the engine this returns `501 not_implemented`, and the UI shows "not implemented yet" rather than an error.

## Constraints

- **Thin client.** No signal recognition/analysis/classification/demod/decode/parse logic in the UI — all of it stays in the backend behind `docs/api.md`. The UI only renders and calls the API. Add a CLAUDE.md rule if not already there.
- **One screen, responsive** to ~400px (phone); theme-aware; no full-page horizontal scroll.
- **Contract-tested:** rely on the T-079 API contract tests; add UI tests for the new components.
- **Reorganise, don't re-append:** the M1 inspector and M2 alarms/reports/scheduler panels move into their proper places (inspector→Decode; alarms/reports→Explore or a review surface), not stacked at the bottom.

## Suggested fan-out (coordinator finalises IDs/deps)

Do a design task first, then parallelise.
- **MUI-DESIGN (Opus high, core_interface):** component + state-store structure, the frontend↔API mapping for every panel, the build setup, and the framework decision (stay dependency-light — vanilla TS + a small reactive store, or a light lib like lit/Preact; justify). Produce a short ADR-0013. Blocks the rest. Keep the waterfall's existing WebGL renderer.
- Then parallel (mostly Sonnet, thin client over docs/api.md; waterfall/DSP-adjacent bits Opus):
  - App shell + mode toggle + Outputs dock + Capture timeline.
  - Explore: inventory (candidate/confirmed, sortable, reachable actions) + selections + focus panel + explanations.
  - Centre: spectrum + waterfall + brackets + region select (reuse existing WebGL).
  - Decode: pipelines + stages + per-stage plots.
  - Decode: packet inspector (reuse T-090) rehomed with linked selection.
  - Wire alarms/reports/scheduler into their places.
  - UI tests + phone-width pass; retire the old stacked layout.

Keep the demo mergeable; the supervisor rebuilds the bears demo when UI changes land.

## Time-bounded signals, a view-scoped Explore, and a History surface (2026-09-16)

The model is **settled by the user** (CLAUDE.md, "Signal & inventory model"); it is recorded in **[ADR-0017](adr/0017-time-extent-signal-model.md)** and the data-model side is [docs/07 §2.27](07-data-model.md). **The staged plan (ADR-0017 §9, stages TM-1…TM-10) awaits the user's sign-off — do not implement ahead of it.** What follows is what MUI looks like once it lands; it amends the panels above rather than replacing them.

### Explore is scoped to the viewed waterfall window

The bug it fixes: the inventory answers "what has **ever** been seen here" while Explore presents it as "what is here **now**", so dead signals pile up as live candidates.

- The LIVE inventory poll sends `t0 = now − waterfall span`, `t1 = now`. **This amends [ADR-0013 §3.3](adr/0013-ui-architecture.md)**, which currently says the inventory is "unbounded in time" while LIVE — that sentence is the bug.
- **Candidates are window-scoped.** A candidate is a hypothesis about energy in the current window; outside it there is no energy to hypothesise about, so the row is simply not listed. Nothing expires and nothing is deleted.
- **Confirmed rows are always listed.** A confirmed emitter is a catalogue entry carrying its own time-presence track. It stays put whether or not it is transmitting, and its **liveness** says which.
- **Liveness** per row, from the API (`presence.liveness`): `live` (on air now), `ended` (with `ended_t_s` — "ended 4 min ago"), `absent` (Confirmed only).
- **Sort by in-window on-air time, never by `count`.** `count` is a lifetime total and belongs to History only.
- Rows show `intervals` and `on_air_s` ("17 events over 6 h, 4.2 s on air"), **never** the `first_seen`→`last_seen` hull as if it were a duration.
- The family may be marked *(from earlier)* when `family_in_window` is `null` — the classification stands, but nothing in this window re-evidenced it.

### Boxes, not brackets

Each in-window row draws a **box** spanning the spectrum trace and the waterfall: `(f_lo..f_hi) × (presence interval ∩ window)`, one per intersecting interval.

- A persisting signal's box **grows** along the time axis as its open interval advances with the live edge.
- A one-off burst's box is a few milliseconds tall and stays that way. Bursts finally look like bursts.
- A chirp gets the **bounding box** of its sweep (ADR-0017 §1.3 — a swept polyline is a later refinement, deliberately not in the plan).
- The **focused** row keeps the existing draggable-edge yellow box from the docs/15 §7 scope above, so the user-band drag (T-191) is unaffected.

### Timeline scrubber and scrub-back

- The scrubber **marks past events** over the capture window from presence intervals, so a burst is visible on the timeline before you scrub to it.
- Scrubbing sets `[t0, t1]` and **re-derives** the lists and the boxes from the same query — one indexed range query, not a detector replay, which is why it stays interactive.
- Waterfall detail below the tile resolution comes from the IQ ring ([ADR-0014](adr/0014-iq-capture-ring.md), 30 min on staging); above it, from `/api/history` tiles. The **lists** come from the interval query and reach as far back as retention allows.

### Span-matched resolution (T-334)

From the user's fourth time/waterfall invariant (CLAUDE.md): **time is zoomable, and the waterfall scales to the selected span.** The visible span runs from seconds to the full retention; the waterfall's rows-per-second is a function of that span; and the backend serves history at a resolution matching it, from the tiered spectrum-history pyramid — **so zooming re-scales rather than truncates.** Data, timestamps and span-matched resolution are the backend's; time↔pixel mapping and view state are MUI's, exactly as pixel↔Hz mapping already is.

What that fixes, and what MUI must therefore not do:

- **Ask in the view's own terms.** The request states `max_t` (rows to draw) and `max_f` (texels across), not only a `max_cells` product. A product budget is satisfied by a grid of any shape, which is how the capture band came to draw 96 bars over 48 h from **two** day-resolution cells: the product bound before the time axis did. Stating the axis budget pulls the hour level instead. (`GET /api/history`, docs/api.md "Span-matched resolution".)
- **Never downsample, never interpolate.** Where a drawn cell covers more than one served cell, the client is choosing which value represents an interval — a measurement, made without the floor, the occupancy threshold or the cell shape, producing a picture that disagrees with the backend's own view of the same span. The served grid errs *coarser* than the view, so the normal operation is **replication** (one measured value drawn across several pixels), never reduction and never a smoothed interpolation between cells.
- **Never infer a timestamp.** Row *k*'s time is `t0_s + k·t_cell_s` from the served grid — the contract the response states, not a count of rows against a wall clock or a nominal row rate. This is the same discipline as invariant 1: boxes and rows share one mapping from absolute capture time, so a box cannot drift out of step with the energy it describes.
- **Never truncate to fit.** Dropping the oldest rows of a response to fit a texture is a silent shortening of the span the user asked to see. The budget goes on the request; a response that could not meet it says so (`resolution.over_resolved`), and MUI surfaces that rather than quietly cutting.
- **Say which horizon.** `GET /api/history` reaches back over the **spectrum-history pyramid's** retention — tiered, lossy, byte-budgeted. The scrubbable capture timeline is sized from the **IQ ring's** window (`GET /api/iqbuffer`), which is shorter and lossless. Invariant 2 exists because those two are different lengths; `resolution.source` names which tier answered so the view can tell live-IQ-backed detail from survey overview.

**Known debt, not an exemption.** The capture band's activity bars still take the max over every frequency cell and normalise against the response's own range, in the client. "Strongest in a band" is precisely what `GET /api/analysis/strongest` exists to keep in the backend, and the pyramid cannot serve it here — its coarsest cell is 100 kHz, so no `max_f` collapses a megahertz-wide band to one column. A band-collapsed *activity-vs-time* series is the backend product that would close it; it does not exist yet and is filed as a follow-up, not waved through.

### History surface (workflow #3)

A **separate surface**, not a tab of Explore: the durable catalogue of every event, one-offs included, browsable by region and time (`GET /api/events`, and `GET /api/inventory/{id}/presence` for one emitter's track).

Nothing is ever deleted from the record to make the live list correct — that is the whole point of splitting the surfaces.

### Listen and decode (the latency ruling)

ADR-0017 §6 rules that **Listen stays live-edge** ([ADR-0011 §8.5](adr/0011-decoder-workbench-contracts.md)) and that "decode only the newly-arrived part" governs **bounded-region** analysis, not live audio.

For the UI this means: a region job and a listener on the same signal are **two pipelines in the dock**, not one. A gap in RDS text beside live audio is correct behaviour (the sibling decode output inherits the audio reader's policy), not a bug to chase — relevant to the RDS readout panel.
