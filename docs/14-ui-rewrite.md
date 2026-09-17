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
- **Two boxes are never drawn stacked, and the client does nothing to arrange that** (T-369). Overlap in time *and* frequency is an error signal in the backend — real emissions do not share a region, and two that did would not demodulate — so `/api/inventory` re-analyses the overlapping region against the detections behind it and either collapses the boxes into the emission they measure as, or records why it could not and leaves both. Either way the list the client draws from does not carry the pair. **The UI must not hide, offset, stack, or z-order overlapping boxes to compensate:** an overlap that reaches the screen is a backend bug to report, not a layout problem to solve, and papering over it would hide the very evidence the re-analysis runs on. `crates/hk-cli/tests/api_contract.rs` asserts the served list has no stacked pair, and the acceptance suite asserts it on the real 45 s off-air capture.

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
- **Say which horizon.** `GET /api/history` reaches back over the **spectrum-history pyramid's** retention — tiered, lossy, byte-budgeted. The scrubbable capture timeline is sized from the **IQ ring's** window (`GET /api/timeline`, over ADR-0014's ring), which is shorter and lossless. Invariant 2 exists because those two are different lengths; `resolution.source` names which tier answered so the view can tell live-IQ-backed detail from survey overview.

**The debt T-334 named here is closed by T-338**, below: the capture band's client-side collapse (max over every frequency cell, plus a min/max normalisation against whatever came back) is gone, replaced by a backend fold and a served dynamic range.

### The timeline is the capture window (T-338)

The user's **second** time/waterfall invariant: *the scrubbable capture-history timeline spans exactly the configured recording/retention duration — no more (not the longer, lossy spectrum-history retention), no less — and grows or shrinks when that duration is reconfigured. It is itself a data display: a compressed "sideways" overview waterfall of the retained capture, never an empty box.*

Two failures were being designed against, and the band had both.

- **The wrong horizon.** The band's span was `WINDOW_S = 48 * 3600`, a hard-coded UI constant, while the IQ ring's retention defaults to two minutes. Every position on it therefore resolved to a time with, in general, **no capture behind it**: a user could scrub to yesterday on a device that holds two minutes. That is worse than an error, because the band renders identically either way. `GET /api/timeline` now serves the window — the ring's configured `retention_s`, the live edge, and (beside it, inside it) what the ring actually holds — and every helper in `ui/src/app/capture/timeline.ts` takes that span with **no default**, so nothing can silently fall back to a constant again. A unit test asserts the absence of the default, not merely the absence of the old constant.
- **An empty box.** The band is the one surface that shows *where activity is worth scrubbing to*, so it has to draw the retained capture rather than frame it. It now renders the compressed overview grid the backend folded, one canvas pixel per served cell, upscaled with `image-rendering: pixelated` — upscaling repeats a measured value across pixels, which is T-334's safe direction; smoothing would invent one between them.

**Why the fold is in the backend and not here.** The pyramid's ladder couples its axes, and a sideways band needs them decoupled: fine in time, coarse in frequency. No level is both, so the client would have had to reduce — and reducing is choosing which measured value a drawn cell stands for, which is a measurement. `/api/timeline` picks the tier from the time axis and folds it onto exactly the `columns × rows` the band draws, carrying only statistics that fold exactly, plus the grid's own observed range so the client does not decide a colour scale either. MUI's share is the shading arithmetic and the pixel mapping.

**The live edge is the capture clock's, not the browser's.** The band used to place everything with `Date.now()`; it now uses the `t1_s` the backend reports, which is where a replay's or a time-compressed scene's own clock lives (invariant 1, §"One shared time axis").

### One shared time axis (T-337)

The user's **first** time/waterfall invariant, and the one the boxes above depend on: *everything time-varying is laid out through one mapping between absolute capture time and screen position, and moves together — waterfall rows, every signal box, selections, the time cursor, the scrubber playhead.* An overlay is anchored in capture time, never at a fixed screen coordinate. **A box drifting out of step with the waterfall's rows-per-second is a violation of this, not a cosmetic bug** (the user's own words).

**The mapping is the rows' own timestamps, inverted.** Each spectrum record arrives with an absolute capture time and a sample index; the waterfall keeps the time with the row; `axis.rowsBackAt` inverts that array. Because a record's `t` is its **first** sample, row *k* covers `[t(k), t(k−1))`, and an emission filling exactly row *k* places on exactly row *k* — `ui/src/axis.ts`, `ui/src/waterfall.ts`.

**What MUI must therefore not do:**

- **Never place anything by a rows-per-second.** The spectrum header's `sample_rate_hz` is a *declared* rate, deliberately up to 10 % above the actual row rate on a gated stream (`RowPlan::declared_hz`), and rows are not evenly spaced in time anyway — a gated row, a dropped run or a backlog-skipped frame advances capture time without advancing the ring. Both errors grow linearly with age, so a box drawn that way walks down the screen away from its energy. A declared period is fine as a *duration* (how long one row stands for: the "↓ 20 s" label's fallback, the newest row's half-open end cap); it is never a placement.
- **One mapping per view, not five.** Before T-337 this surface had five: the shader's ring index for the rows, a nominal rate for the presence boxes, the exact per-row lookup for hover and drag, wall clock for the scrubber, and a column index for the activity bars. Rows and boxes disagreed by construction, and a region dragged out of the waterfall and drawn back did not land where it was dragged. Rows, presence boxes, timed selections and the drag draft now all go through `RowClock` (`ui/src/app/centre/overlays.ts`).
- **And one *clock*, not two (T-362).** One mapping is not enough if it is evaluated on the wrong schedule — see the section below.
- **Never fabricate a placement.** A record whose span has scrolled off the rows held draws nothing, rather than a box clamped to a height it never had — the same rule `presenceBoxes` already applied, now applied to selections too.
- **Measure the axis label, don't assert it.** "↓ 20 s" is the span the rows on screen actually cover, not `rows × declared period`.

**Still screen-anchored, deliberately:** the focused Confirmed row's full-height yellow band box (T-193's drag-to-adjust edges live on it) is a *frequency* tool and spans both panes by design; every other row draws the time-placed presence box instead (ADR-0017 TM-4).

**Known debt, named:** the capture timeline's half of this is **closed by T-338** (above) — it places by the backend's reported live edge over the ring's retention, not by `Date.now()` over a 48 h constant. The Explore Candidate query still derives its window from a nominal row rate (`ui/src/app/explore/inventory.ts`); that one remains filed, not fixed.

### The boxes are drawn in the waterfall's render pass (T-362)

The user, from live testing the day T-337 landed: *the confirmed/candidate presence boxes **drift then jump**.* This is **not** a regression against the section above. It is the same invariant, sharpened: T-337 made a box's placement a pure function of absolute capture time, and the mapping was correct — but it was **evaluated on the ~1 s inventory poll** while the WebGL2 waterfall scrolls **every animation frame**. Between polls a box stood still while the rows scrolled out from under it; at the poll it snapped back. The invariant held instant by instant and failed over time.

**A right mapping on the wrong clock is the same defect as a wrong mapping**, and it is the failure T-337's own measurement should have predicted: a discipline ("place everything through `RowClock`") is kept by remembering, and remembering is what had just failed. The fix the user asked for is therefore structural, not procedural.

**What changed.** A time-varying overlay is no longer a DOM rectangle. It is a `TimeBox` — a band fraction (0..1 over the bins in order, the *same* coordinates `Waterfall.setView`'s zoom window is in) and two absolute capture times — and it carries **no screen position at all**. The waterfall takes the list (`Waterfall.setBoxes`) and, inside its own `frame()`, in the pass that has just drawn the rows and in that pass's viewport, converts each one to a rectangle through `axis.rowsBackAt` over the rows' own timestamps (`ui/src/timebox.ts` `placeTimeBoxes`, `ui/src/waterfall.ts` `drawBoxes`). Supplying boxes is a **data update**; it is not a placement, and a poll that changes nothing changes nothing on screen.

Why that cannot desync: there is no stored pixel position to go stale. The box's y comes from the same `this.head` and the same `this.times` the row shader's `uHead` came from, three statements earlier in the same function; its x comes from the same `this.u0`/`this.u1` the row shader was handed. Desync would require the renderer to disagree with itself inside one call.

**The dividing line, and it is the rule for anything added later:** *an overlay with a time extent is drawn in the render pass; an overlay without one stays DOM.* So the presence boxes and **timed selections** moved into the canvas; the focused Confirmed row's full-height band box (T-193's drag-to-adjust edges) and a **frequency-only** selection stay DOM, because neither has a time axis to sit on and neither can drift. The `.c-presence-box` / `.c-presence` CSS is gone, and so is `axis.timeSpanY` — a placement helper in canvas fractions with no consumer left is how a second layer gets started.

**Interactivity, without a second opinion about position.** The presence boxes never had DOM interaction (`.c-presence { pointer-events: none }`); their only affordance was a `title`, and clicks on the waterfall have always resolved by *frequency* (`clickTarget`), not by DOM hit-testing — the same path a right-click over a T-193 band box already took. So the readout carries the description now, resolved by `Waterfall.boxAt(u, tS)`: **containment in the boxes' own two axes** at the pointer's own row time, not a test against a remembered rectangle. It therefore names the box actually under the pointer however many rows have scrolled since the last poll. No layer retains its own idea of where a box is.

**How it is tested** (`ui/test/timebox.test.ts`, driving the real `Waterfall` through a recording WebGL2 stub, so the assertions are on the rect the pass submitted):

- **The property — between polls.** Advance the waterfall by *N* rows with **no** `setBoxes` at all; the box must have moved by exactly *N* rows of the rows' own mapping. A test that only checked placement at poll boundaries passes on the broken code.
- **The control — no jump.** A poll delivering an identical interval must produce a byte-identical rectangle. That is the "jump" half of the bug, and it is what proves a poll is no longer a placement event.
- **Paused** (T-339: pause freezes the view, never the capture): rows stop being pushed while the ring keeps filling, so the head holds — and the box holds with it, on the rows **displayed**, across 30 frames and several polls.
- **Zoomed:** the box goes through the zoom window the row pass was handed in that same frame, and a zoom with no poll in between rescales it exactly as it rescales the rows.
- **Uneven rows:** a 1.2 s gated gap puts a nominal 25 rows/s 29 rows adrift on a 512-row pane; the box follows the ring.
- **Mutation-checked.** Reverting to poll-rate evaluation fails the between-polls property and the zoom case, while the no-jump and paused controls keep passing — which is exactly the discrimination those controls are for.
- **Guarded:** `src/timebox.ts` is asserted to contain no dB/SNR/occupancy term, no 6+-digit literal, and no rate (`rowRate`, `rowsPerS`, `rowPeriod`, `sample_rate`, `Date.now`) — a nominal rate would look right for the first second after every poll and drift linearly with age, which is this bug reintroduced.

**Nothing new was needed from the backend.** Every record involved already carries absolute capture time (T-337, docs/07 §4.2); this was entirely a question of when the client evaluated it.

### Setting centre is a device action, not a view change (T-343)

Panning, zooming, scrubbing and pausing change what is drawn. **Setting the centre frequency moves the radio**, and the two are not interchangeable — the user names this as the asymmetry the navigators have to carry, and it is the exact opposite of T-339's invariant that pause never touches the device.

A retune re-derives the window's content class and, when the class or sample rate changes, **stops and re-plumbs the running segment** (`PipelineController::retune`), tearing down and restarting its always-on readers. It also takes the one radio: only one process can open an SDR, so the UI is one claimant among others (a HIL run, a scheduler survey, the user's own demo server), not a privileged one.

**The defect this replaced.** `ui/src/controls/gestures.ts` used to call straight through to the control API's centre route on `pointerup` whenever an accumulated pan passed `OVERFLOW_FRAC = 0.05` of the view width. A 5 % threshold inside one gesture handler was the only thing separating a view change from a device command — no distinct gesture, no type, no confirmation — so **a pan let go slightly too far could stop and restart capture**. T-339's audit found it precisely because nobody had classified that path as dangerous.

**What replaced it:**

- **A pan offers, it never commands.** Running off the band edge leaves a `RetuneOffer` in the `live` slice and draws a button on the frequency axis (`Retune to 99.5000 MHz`, titled with the device it would move and the note that panning and zooming do not). Pressing it is the explicit user action. A new stream header clears a stale offer.
- **A type, not a convention.** `ui/src/app/centre/view.ts` exposes `DeviceAction` and `applyDeviceAction`, the only path in the client to a device route besides the SDR control panel (`ui/src/app/review/device.ts`). `DeviceAction` values are built only by explicit user requests — Go to, a bookmark jump, an accepted offer, and in future the frequency navigator (`source: "navigator"`). A gesture cannot build one, and `ui/test/app-centre.test.ts` asserts against the source that no gesture module names a device route.
- **The backend says which requests reach the radio.** `Action::device_action` (hk-api) is an exhaustive classification, so a new route must choose a side; a device action's answer and audit entry carry `device: {action, id}` with the front end's provenance `device_id`, and `/api/control/state` reports the same id so the UI can name the radio before it moves it. See [docs/api.md § Device actions](api.md#device-actions-t-343).
- **One at a time.** Device actions serialise on one `DeviceGate`; a contended one answers `409 device_busy` naming the holder rather than racing it to the driver. The UI reports that; it never retries into the race.
- **Never automatic.** No code path retunes without a user asking. Closed-loop refinement (`docs/14` "tune from the processed output") adjusts a *channel* inside the tuned window, not the front end.

**The frequency navigator is T-340**, below; `DeviceAction`'s `source: "navigator"` is the variant it uses.

### Each waterfall axis has an edge navigator (T-340)

The user's **fifth** time/waterfall invariant: *time runs down the waterfall and frequency across it, so the **time navigator is a vertical bar on the side** (an overview of the retained capture window) and the **frequency navigator is a horizontal bar along the bottom** (spanning the whole surveyed / device-available spectrum, setting the centre). Each navigator pans and zooms its own axis; a dragged region on either zooms the main view to it. The frequency navigator shows every currently-active capture window as a lit segment — the natural home for multiple SDRs and for survey/sweep coverage.*

**The split, stated by the user:** *"Backend reports the achievable (centre, span) grid + full-spectrum survey overview; UI does the navigators/gestures/snap/styling."* So the two bars are `ui/src/app/centre/navigators.ts` (mount, gestures, styling) over `ui/src/navigators.ts` (pure placement arithmetic), and every number they place came from a backend answer: the grid from `/api/navigation` (T-341), the capture window and its overview from `/api/timeline` (T-338), the active window list from `/api/navigation`'s `windows` (below).

| Gesture | Frequency bar (bottom) | Time bar (side) |
|---|---|---|
| **pan** | drag the view marker: moves the main view's frequency window inside the tuned band, clamped at its edges | drag the marker: moves the reviewed instant inside the capture window |
| **zoom** | wheel: zooms the main view about the pointer | wheel: zooms the reviewed span |
| **drag a region** | zooms the main view to it, snapped through `snapState` | reviews exactly that span, at the tier `snapTimeCell` names |

**Three things the bars are not allowed to do**, each with a test in `ui/test/navigators.test.ts`:

- **Move the radio.** A pan or a zoom is a view change. A region dragged *outside* the tuned band cannot be shown without retuning, so it leaves a `RetuneOffer` in the store — the same offer a pan to the band edge leaves (T-343) — and the device moves only when the user presses the button, through `applyDeviceAction` with `source: "navigator"`. The control asserts that panning either bar across its **whole** extent (a 6 GHz drag, thousands of times the tuned window) reaches no device route, while the view still moves and the offer still appears; a second test asserts the navigator module names no device route at all.
- **Size the time bar from the wrong horizon.** Its extent is `GET /api/timeline`'s `window` — the IQ ring's configured retention — so every position on it has capture behind it. The control reconfigures the retention and watches the extent follow, and a source assertion holds that `ui/src/navigators.ts` never reads `latest_s`, `max_age_s` or the pyramid's cell bounds: those describe the *spectrum-history* horizon, which is longer, lossy, and not this bar.
- **Assume one capture window.** The lit segments come from the reported `windows` list and nothing else. The control feeds a body carrying `frequency.current` but **no** list and asserts **nothing** lights: a one-element list derived from the tuned state would be a window count nobody measured. Two reported windows draw two segments, in the order given, unmerged.

**The backend half: `windows` (T-340).** `GET /api/navigation` now reports every currently-active capture window as a list — `device_id`, `driver`, centre, span and the window's edges per entry — empty on a replay, one entry on a live run. The count is measured per request, not a constant: the source layer is already N-shaped (T-259's audit; T-302/T-303/T-304/T-305), and multiple simultaneous front ends are an explicit product direction. **What would have to change to report N:** `ApiState::live_control` is one `Option<Arc<dyn LiveControl>>`; it becomes a collection built one handle per `ReceiveChain` where the pipeline composes the run. Neither the route's shape nor its clients change, because both already speak in lists. Multi-device capture is *not* built and the field never claims it is. The contract test asserts the array by value and then **retunes the mock and watches the window move with it**, so a constant entry — or one copied from the run's configuration — fails.

**Time zoom became real state.** "A dragged region zooms the main view to it" needs somewhere for a time span to live, and the review cursor had only an instant. `TimeCursor` gained `spanS`, which `historyWindow` uses when it is set; with none asked for the window is still the rows on screen at their own period. It is never defaulted to a duration — that is the 48 h constant T-338 removed, in another costume.

#### The two bars control *different* axes (T-367)

The user's correction to the invariant above, and a defect in T-340: *"the **left vertical bar is the TIME navigator**: it selects the time range and shows a compressed history waterfall **of the currently-selected frequency range only** (not the whole spectrum) — it never changes frequency. The **bottom horizontal bar is the FREQUENCY navigator**: it sets the centre and span (the 'survey' across the whole device-available spectrum) and shows the most-recent sample/occupancy across that range — it never scrubs time. **(Wiring both bars to scrub time is a bug.)**"*

What T-340 actually shipped, checked against the source rather than against its own report:

- **The gestures were already one-axis each.** Every frequency-bar gesture wrote `live` (`setLiveView` / `setRetuneOffer`); every time-bar gesture wrote `time` (`goLive` / `reviewAt`). Neither bar scrubbed the other's axis. What was missing was any *guarantee* — the property held by inspection, and nothing would have failed if a later edit crossed the wires.
- **The time bar's picture was of no frequency range at all.** Its poll asked `GET /api/timeline?columns=…&rows=…` with **no `f_lo`/`f_hi`**, and that route answers `grid: null` when no region is given (a picture of "everything" is a different measurement, not a default). So the vertical bar — whose entire content is *what has been happening here over the retained window* — drew an empty canvas. Not the whole spectrum: nothing.

T-367 fixes both:

- **The overview is scoped to the selected frequency range and follows it.** `timelineRequest` (`ui/src/navigators.ts`) builds the request over the range the main view is on (`currentSpan`: the zoomed view, falling back to what the device is tuned to), and the mount re-asks when that range changes, coalesced by `BAND_SETTLE_MS` so a drag is one request rather than a hundred. A `seq` guard drops a stale answer, because drawing one range's energy on a bar labelled with another's is the same class of lie as not scoping at all. **No backend parameter was needed** — `/api/timeline` has taken `f_lo`/`f_hi` since T-338; the client simply never sent them. With no range selected the bar asks for no picture rather than widening to the spectrum.
- **Each bar is confined to its own axis by construction.** Time gestures resolve to a `TimeTarget` — a time and a span, a type with no shape that could name a frequency — applied by `applyTimeTarget`, which writes the `time` key and only that. Frequency gestures resolve to a `FreqZoom` applied by `applyFreqZoom`, which writes `live` and only that.

**The property and its control.** The property is a pair: the time bar's overview **changes** when the selected frequency range changes, and the frequency bar's content does **not** change when the time selection changes. The control is stronger than "looks right" — after a gesture on either bar, the *other* axis's slice is **the same object** (`Object.is`, not deep equality), and a source test splits `navigators.ts` at its two mounts and asserts that `mountTimeNav` names no frequency writer (`setLiveView`, `setRetuneOffer`, `applyDeviceAction`, …) and `mountFreqNav` names no time writer (`reviewAt`, `goLive`, …) — the exact bug the user named. A further assertion forbids the unscoped request shape from ever reappearing. On the backend half, the contract test asks the same server for two disjoint halves of one band and asserts the grid's frequency origin, cell width and served span follow the request while `window.span_s` does not move: **the time extent is not a function of the band; the picture is.**

**What is still missing, and why it is not here.** The frequency bar shows lit capture windows but not yet "the most-recent sample/occupancy across that range". A survey strip over the whole device-available spectrum needs the **coverage map** (T-368) first: without one, unobserved spectrum would be painted as *quiet*, which is the exact failure the "grey means genuinely unobserved" invariant forbids. The user sequenced T-368 third, so the bar stays honest and empty there until it lands.

### Navigation snaps to achievable states, and the view says what it is showing (T-341)

**The rule, from the user** (CLAUDE.md, "Time, the waterfall, and the live view", invariant 6): *navigation is discretized to achievable capture states, and the UI never implies detail the front end can't deliver.* Zoom/pan and region-select resolve only to **realizable** configurations and snap to the nearest one; wider than the live window is **survey-history overview, not live IQ**; the view must distinguish live-IQ-backed detail from overview.

This is the exploration-first honesty principle applied to navigation — the same rule that makes the band-plan database a suggester rather than a source of truth. **An interpolated pixel that looks like a measurement is a lie with a picture attached.**

**The split, stated by the user:** *"Backend reports the achievable (centre, span) grid + full-spectrum survey overview; UI does the navigators/gestures/snap/styling."* So:

| Backend (`GET /api/navigation`, `hk-api/src/navigation.rs`) | Client (`ui/src/navigation.ts`) |
|---|---|
| which centres exist (the **tuning step**), which spans a device can open, which time cells the history holds | finding the nearest point on that grid |
| whether a requested state is live-IQ backed or survey overview (`resolved.source`) | styling the distinction, and the gesture that produced the request |

`ui/src/navigation.ts` is pure arithmetic over the grid it was handed, the same class of work as `axis.ts` — never a second opinion about what the radio can do.

**The axis that was missing.** T-343 named it precisely: `/api/control/state` already reported `frequency_ranges_hz` and `sample_rates_hz`, but `SourceCapabilities` had **no tuning step**, so the achievable grid was two of its three axes. T-341 added it at the source layer, device-generic, and **three-valued** like the bias tee: `"uniform"` with a step, or `"unknown"` when the source cannot say. A HackRF One reports `30 MHz / 2²⁰` = 28.6102294921875 Hz — the MAX2837 fractional-N granularity, not the 1 Hz its USB API accepts, because 28 of every 29 such commands land the LO on the same synthesiser point and a declared 1 Hz step would draw 28 imaginary centres on the axis. A SigMF replay reports `"unknown"`, and then **nothing snaps**: an unknown grid has no nearest point, and echoing the request back would claim the device can sit exactly there.

**Where the client uses it today.** `applyDeviceAction` (`ui/src/app/centre/view.ts`) snaps a retune's centre to the grid before it posts. It used to `Math.round` to a whole hertz — a number the front end does not have. Everything else the module offers is for T-340's navigators and T-338's timeline, which are the surfaces where a gesture becomes a capture state; a zoom *inside* the tuned band is a display zoom over live IQ and correctly snaps to nothing.

**The live-vs-overview claim is on the wire, not inferred.** T-334 shipped `resolution.source` as the constant `"spectrum-history"`, documented as the home for "which tier answered"; T-341 gave it its other two values, `"live-iq"` and `"survey-overview"`, plus a `live` boolean and a backend-rendered `statement`. `/api/history` never claims `live-iq` — it reads the pyramid and only the pyramid — but it does say `"survey-overview"` when the span it served could not have fitted one capture window. A client styles the difference; it never decides it.

### Grey means genuinely unobserved, and the frequency bar has a viewport (T-368, T-376)

The honest half of the rule above. T-341 stopped the view claiming detail the front end never captured; this lets it *show* what the front end did capture, and grey only what it did not.

- **Three states, three treatments.** Observed-with-energy is the colour ramp; observed-and-quiet is the ramp's low end — a real finding; **never observed is grey**. A fourth, *observed but no level retained*, is a flat tint: neither grey nor the ramp's bottom. The client decides none of this: `GET /api/coverage` serves `state` per cell and `resolution.grey_rule` states the rule, and an unobserved cell carries no measurement key the client could read as a zero.
- **The frequency navigator's survey strip is that coverage.** The bar spans the device-available spectrum, most of which the radio has never been tuned to; filling it from energy alone would paint never-observed spectrum as quiet, which is exactly the failure the invariant forbids. So the strip's cells come from the coverage map, and the bar re-asks over whatever range it is currently showing.
- **The bar has a viewport of its own (T-376).** Its extent used to be the whole reported spectrum, fixed — on a 1 MHz–6 GHz front end a 2.4 MHz capture window is four ten-thousandths of the bar, invisible rather than off-centre. It now opens **centred on the current tune centre**, a few tens of capture windows wide, and the **wheel zooms that viewport** about the pointer with the same `wheelFactor` the waterfall uses. Zooming out toward the whole range is exactly when the coverage map has to be right.
- **An untouched viewport follows the tune; a framed one does not.** The default frame answers *where am I*, so it moves when the radio does. Once the user has deliberately zoomed, re-centring under them would undo the gesture they just made, so a touched viewport is only clamped back into the device's reported bounds.
- **No gesture on either bar moves the radio.** The wheel writes the bar's own frame and no store slice; panning the view marker moves the main view; only the region drag leaves a **retune offer**, and only pressing it reaches the device (T-343). T-340's control — drag ±1.0 of the full bar through a spy client and assert nothing was called — still holds unchanged.
- **Backfill.** Switching to Live for a range that has history starts populated from the pyramid rather than black, because the coverage map says the range was observed and the history has the cells to draw. *(Written here as the intent; the waterfall itself still went black until T-379 implemented it — see below.)*

### The whole UI is one window, and must show everything it has for it (T-379)

**The rule, and it applies to every surface, not to the one that was last found wrong.** The UI presents a single **(time range × frequency range)** window. Every surface — the waterfall, both edge navigators, the inventory Candidate/Confirmed lists, the focus panel, the output/decode panels, the capture band — is a *view over that one window*. Whenever data exists for the window (live, the IQ ring, the spectrum-history pyramid, prior surveys) it must be shown. A surface may render grey or empty **only where data genuinely does not exist**. *"We have it but didn't render it" is a bug* — the black Live waterfall, the fixed-size grey block and the empty sidebars were three faces of it.

Four obligations follow, and a surface is held to all four:

1. **Ask about the window, on the capture clock.** The live edge is the *capture* clock's: the newest spectrum row's own timestamp, else `GET /api/timeline`'s capture window, else **unknown**. `Date.now()` is never an answer and never a fallback. A replay, the mock SDR on a time-compressed scene, and any source whose stamps are not the host's run on a clock of their own — the fixture that exposed this sat **3.5 days** from wall time, so the Candidate list's 20 s window ending at browser-now selected nothing while five candidates stood in the store and four of them fell inside the very same 20 s of capture. An empty list produced that way is *empty-because-not-fetched*, and it is indistinguishable on screen from a quiet band.
2. **One window, not one per surface.** The Candidate list's window is the waterfall's own `historyWindow` — the span a time-navigator drag asked for, else the span the rows on screen cover. It used to be a flat `REVIEW_WINDOW_S = 3600` while reviewing, which is a *second* window: the list answered about an hour while every other surface answered about the twenty seconds under it, listing candidates that were nowhere on the waterfall and, once a longer span was dragged, omitting ones that were. The frequency navigator's survey carries the same window's `t0`/`t1` for the same reason: without them it answered about the live edge while the waterfall above it showed an hour ago, greying bands that *had* been observed then.
3. **Say which emptiness it is.** "Nothing was observed here" and "nothing was on the air" are different claims and one of them is not a finding at all. The sidebar distinguishes four states — *no window known* → "Waiting for the capture window…"; *window unobserved* → "Nothing was observed in this window — no data, not a quiet band."; *window observed* → "Nothing on the air in this window."; *coverage unknown* → "Nothing listed for this window." Only the third is a statement about the air. The observed/unobserved claim comes from `GET /api/coverage` for **exactly** the list's own window, reusing `hk_store::Coverage` rather than inventing a parallel notion of emptiness; an answer that never came stays *unknown* and never hardens into "unobserved".
4. **Never widen the window to look full.** The tempting non-fix is to make a sidebar non-empty by relaxing what it shows — stale rows, or all-time rows. That satisfies the letter and breaks time-scoping. **The fix is always to fetch and render what exists for the window.** When the window cannot be named, the correct behaviour is to send *no query at all* and say so, not to send a plausible one.

**The same rule for a focused signal, not just an empty list (T-385).** Obligation 3 is about *which* emptiness, and the focus panel beside the sidebar had only one sentence for every absence: a row missing from `inventory.rows` rendered as **"That signal is no longer in the inventory."** For a window-scoped inventory the ordinary absence is having scrubbed or retuned away, so the sentence asserted a deletion the UI had never observed — a false claim about the user's own data, which is the exploration-first failure aimed at a panel rather than at a band plan. An emitter outside the window is **unobserved-here, not deleted**.

The panel now resolves the absence instead of guessing at it, in the same vocabulary: *deleted* ("That signal was deleted from the inventory."), *gone* — a `404`, no such entry ("That signal is no longer in the inventory.", now said only where it is true), *outside the window on screen* ("Not in the window on screen — outside the time or frequency range you are viewing, not gone.", and over an unobserved window "…nothing was observed here, so it is unobserved, not gone."), *no window known* (T-379's first sentence verbatim), *checking*, and *unchecked* when the lookup never answered — which stays unknown rather than hardening into "deleted", the same rule as for a coverage answer that never came.

Two things make it honest rather than merely reassuring. **The claim comes from a measurement, not from the absence**: `GET /api/inventory/{id}` serves deleted entries with their lifecycle state, so the entry's own `state` says which case it is; nothing is concluded from the row not being in the list. And **the mirror image is a bug too** — turning every absence into "outside the window" would hide a deletion the user really made, so the two are asserted to produce different observable states, pairwise, the way T-379 asserted its emptinesses. The lookup is cached **by the window as well as by the emitter id** (a row is absent *of a window*) and re-asked whenever the row set is republished, which is what settles the race against T-187's optimistic delete.

**Backfill, actually implemented.** `wf.reset()` fills the ring with `-1e30`, which is not the `UNOBSERVED_DB` sentinel, so the shader painted the bottom of the colour ramp — the literal black Live waterfall. Going Live, and retuning, now re-ask `GET /api/history` over `[live edge − span, live edge]` and load the rows before live rows push on top. Because rows carry the served grid's own times, the history and live rows sit on one absolute-time axis (§"One shared time axis"), so nothing is placed from a period assumed in the client. It is best-effort: with no live edge, no geometry or no history the ring stays as `reset()` left it — the old behaviour, never a worse one.

**The output and decode panels, the other half (T-384).** T-379 held the waterfall, both navigators and the Candidate list to the four obligations above and listed the surfaces it had not reached. The output/decode panels were the largest of them, and their failure is the *inverse* of the empty sidebar's, which is why it survived a live-visible bug hunt: a panel scrubbed back an hour did not go blank — it went on rendering the **live edge's** RDS station name, RadioText, PTY and frame tallies under a past window's heading. Wrong-window data reads as right.

The root was in the API, not the client: `GET /api/inventory/{id}/decode` **had no time parameter at all**, so no correctly-wired panel could have asked. It now takes the same `t0`/`t1` as every other windowed route, and the panels send `viewWindow()` — obligation 2, one window, extended to the surfaces that had none.

Two things that had to be settled rather than assumed:

- **Which route carries the window.** `/api/captures/{id}/frames` already has `from_t`/`to_t`, and adding a second window to it would have been the cheap move. It is the wrong one: that route is keyed by *capture* and serves raw stream records, while these panels are keyed by *emitter* and render committed fields — there is no emitter→capture key in the store to follow, and re-assembling a station name out of raw RDS group records in the client would be both a thin-client violation and a re-decode. So `/decode` got the window (emitter-keyed, committed fields) and `/captures/{id}/frames` kept its own (capture-keyed, records) — the decode workbench's frame tallies use the second, which is what it was already for.
- **Re-deriving must not re-decode.** CLAUDE.md's decode invariant says live decoding *extends* a region and decodes only the newly-arrived part. A window on `/decode` filters rows the decoder already committed by their own capture-clock `at`, so no decoder runs to answer a scrub. Latest-per-frame-model is computed **after** the filter: the other order answers "nothing decoded here" for a window that plainly holds an older row.

And the same bug T-379 killed was still alive here: `ui/src/app/decode/plots.ts` windowed its frame buffer on `Date.now() * 1e6` at three sites, comparing a browser instant against capture-clock `t_ns`. On the replay behind T-379 that is a 3.5-day gap, so the "last 60 s" CRC tally was permanently empty — and with the offset the other way it would have silently become an all-time tally labelled 60 s. Those sites now take the view window; the live buffer is bounded by *count* rather than by a clock (a time-trimmed buffer discards live frames while the view is scrubbed back, so returning to Live would find the plot missing frames it had already received); and a window the socket never carried is backfilled from the pipeline's own capture, because those frames exist and the rule is that data which exists must be shown.

### History surface (workflow #3)

A **separate surface**, not a tab of Explore: the durable catalogue of every event, one-offs included, browsable by region and time (`GET /api/events`, and `GET /api/inventory/{id}/presence` for one emitter's track).

Nothing is ever deleted from the record to make the live list correct — that is the whole point of splitting the surfaces.

### Listen and decode (the latency ruling)

ADR-0017 §6 rules that **Listen stays live-edge** ([ADR-0011 §8.5](adr/0011-decoder-workbench-contracts.md)) and that "decode only the newly-arrived part" governs **bounded-region** analysis, not live audio.

For the UI this means: a region job and a listener on the same signal are **two pipelines in the dock**, not one. A gap in RDS text beside live audio is correct behaviour (the sibling decode output inherits the audio reader's policy), not a bug to chase — relevant to the RDS readout panel.
