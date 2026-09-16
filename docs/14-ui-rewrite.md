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
- **Never fabricate a placement.** A record whose span has scrolled off the rows held draws nothing, rather than a box clamped to a height it never had — the same rule `presenceBoxes` already applied, now applied to selections too.
- **Measure the axis label, don't assert it.** "↓ 20 s" is the span the rows on screen actually cover, not `rows × declared period`.

**Still screen-anchored, deliberately:** the focused Confirmed row's full-height yellow band box (T-193's drag-to-adjust edges live on it) is a *frequency* tool and spans both panes by design; every other row draws the time-placed presence box instead (ADR-0017 TM-4).

**Known debt, named:** the capture timeline's half of this is **closed by T-338** (above) — it places by the backend's reported live edge over the ring's retention, not by `Date.now()` over a 48 h constant. The Explore Candidate query still derives its window from a nominal row rate (`ui/src/app/explore/inventory.ts`); that one remains filed, not fixed.

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

### History surface (workflow #3)

A **separate surface**, not a tab of Explore: the durable catalogue of every event, one-offs included, browsable by region and time (`GET /api/events`, and `GET /api/inventory/{id}/presence` for one emitter's track).

Nothing is ever deleted from the record to make the live list correct — that is the whole point of splitting the surfaces.

### Listen and decode (the latency ruling)

ADR-0017 §6 rules that **Listen stays live-edge** ([ADR-0011 §8.5](adr/0011-decoder-workbench-contracts.md)) and that "decode only the newly-arrived part" governs **bounded-region** analysis, not live audio.

For the UI this means: a region job and a listener on the same signal are **two pipelines in the dock**, not one. A gap in RDS text beside live audio is correct behaviour (the sibling decode output inherits the audio reader's policy), not a bug to chase — relevant to the RDS readout panel.
