# ADR-0013 — MUI web UI architecture: framework, component tree, state store, API map, migration

**Status:** PROVISIONAL (T-149, core interface, reviewed before merge)
**Touches:** the web client (`ui/`); [ADR-0002](0002-ui-web-vs-native.md) (web UI, thin client over the same API as external programs); [`docs/api.md`](../api.md) and its contract tests (`crates/hk-cli/tests/api_contract.rs`, T-079); [`docs/stream-contract.md`](../stream-contract.md) §5, §10, §12, §14; brief [`docs/14-ui-rewrite.md`](../14-ui-rewrite.md); spec `ui/mockups/explorer-v3.html`.
**Code:** skeleton in `ui/src/app/` (`main.ts`, `store.ts`, `state.ts`, `shell-slice.ts`, `net.ts`, `dom.ts`, `context.ts`, `shell.ts`, `placeholder.ts`, `index.html`, `app.css`, `base.css`; per area `<area>/{index.ts,slice.ts,<area>.css}` for `explore`, `centre`, `capture`, `dock`, `decode`, `review`; `centre/live-spectrum.ts`; stub contracts `dock/api.ts`, `decode/status-feed.ts`; `decode/{inspector.ts,inspector-slice.ts,inspector.css}`), tests `ui/test/app-store.test.ts` and `ui/test/app-shell.test.ts`, runner `ui/test/run.mjs`. Served at `/` (built to `dist/index.html`), aliased at `/app.html`; the old stacked UI (`main.ts`, `index.html`, `style.css`) was retired by T-156.

## Context

The M0b–M2 panels were appended to one page as they landed, so the UI scrolls a long way and doesn't fit the product's exploratory workflow. The user approved a one-screen redesign (MUI, docs/14). The backend is already API-first and contract-tested, so only the frontend changes.

Constraints carried in:
- **Thin client** (CLAUDE.md "Working conventions"). `ui/src` renders, handles interaction, formats, maps axes and calls the API. Recognition, analysis, classification, demodulation, decoding and parsing stay in the backend.
- **One developer**, plus Sonnet agents doing most panel work in parallel. The structure has to be obvious, and file ownership has to split cleanly.
- **Existing assets to keep:**
  - the WebGL2 waterfall (`waterfall.ts`, 60 fps at 16k bins);
  - the audio worklet and jitter buffer (`audio-worklet.ts`, `jitter.ts`, `audio-frames.ts`);
  - pure, tested helpers in `axis.ts`, `controls/{client,model,freq}.ts`, `inventory.ts`, `selections.ts` (`SelectionStore` with offline sync), `frame-inspector.ts` (tree, byte↔field linking), `alarms.ts`, `report.ts` and `scheduler.ts`.
- **Build and test harness:** esbuild bundles, `tsc --noEmit` typechecks, and tests are `node:test` files bundled by esbuild. There's no DOM under test.
- **The server sets `Content-Security-Policy: default-src 'self'`,** so there are no CDN scripts or web fonts, and the mockup's Chivo font falls back to system stacks. Static files come from `ui/dist` by safe relative path, so `/app.html` works with no backend change.
- **Target devices:** a portable device down to phone width (~400 px), in either theme.

## Decision summary

| Question | Decision |
|---|---|
| Framework | **Vanilla TypeScript** plus the ~60-line store in `ui/src/app/store.ts` and the `h()` element helper. No runtime dependency. |
| Budget | Runtime dependencies: **0**. Dev dependencies: esbuild and typescript only; at most one DOM-shim dev dependency (e.g. `linkedom`) may be added by T-156 if a DOM test needs it. `dist/app.js` ≤ **150 KB** minified (≤ 45 KB gzip); `dist/app.css` ≤ **40 KB**. The skeleton measures in §1. |
| Rendering | Each panel's `mount(el, ctx)` owns one `data-slot` subtree. Panels re-render from `store.select` on their own slices, using `h()` and `textContent` (never `innerHTML` with API strings). High-rate data bypasses the store: spectrum rows, PCM, per-frame meters. |
| State | One immutable `AppState` with slices (§3). Actions are pure `(state) => patch` functions. Cross-panel communication goes only through the store. |
| Transport | `ControlClient` (bearer header) for HTTP. `net.openStream` handles WebSockets with the §10 framing and refusals. `startPoll` polls without overlap and backs off on failure. Reconnect uses `backoffMs` (250 ms doubling to 10 s). |
| API gaps | 14 entries in §4.9, numbered 1–13 (7 is split into 7a and 7b). Each becomes a proposed backend task. The UI ships an interim behaviour and never computes the missing thing itself. |
| Migration | New app at `/app.html` next to `/`. Tasks T-150…T-155 fill slots in parallel. T-156 makes `/` the new app and deletes the DOM-bound old modules. |

## 1. Framework

**Chosen: vanilla TypeScript, a small store, and DOM helpers.** Candidates compared:

| | Vanilla + store | Preact (+signals) | lit | React / Svelte / Vue |
|---|---|---|---|---|
| Runtime size (gzip) | 0 (store ≈ 0.5 KB) | ≈ 5–6 KB | ≈ 6 KB | 45 KB / compiler / 35 KB |
| New dependencies | none | 2 packages; JSX config for esbuild and tsc | 1 package plus decorators or static properties | many, or a compiler toolchain |
| Test harness | unchanged: pure actions and helpers under `node:test` | needs a DOM shim for component tests | needs a DOM shim, and Shadow DOM complicates it | new runner |
| Waterfall / worklet reuse | direct: both are imperative objects on a canvas or `AudioContext` | wrapped in refs and effects | wrapped in lifecycle callbacks | wrapped |
| Theme tokens | one global stylesheet | same | Shadow DOM blocks global CSS unless styles are re-imported | same as Preact |
| Old helpers reused | imported as-is | as-is | as-is | as-is |
| Who maintains it | one developer; nothing to upgrade | small, but a moving target | small, but a moving target | heavy |

The mockup's interactions don't need a virtual DOM:

- the lists are short and re-rendered whole: inventory ≤ 500 rows per page, frames ≤ 60 visible, pipelines and stages ≤ 20;
- the hot paths (waterfall, spectrum, audio, meters) are canvas or WebGL drawing driven by `requestAnimationFrame`, which any framework would have to escape anyway.

A library would add dependencies and build config but remove little code. Preact is the fallback if T-156 finds per-panel re-render code has grown unwieldy. Switching later stays cheap because panels already own isolated subtrees behind `mount(el, ctx)`.

**Rendering rules:**
- A panel re-renders only its own subtree, from `store.select(selector, fn)`.
- Lists rebuild with `replaceChildren` and keep focus and scroll by id (`data-id`); keyed patching is for lists > 500 rows only.
- No work runs per animation frame except in canvas renderers. Meters and rates are written to the store at ≤ 4 Hz, and a meter's bars are updated by its own element's rAF, not by the store.
- API strings go through `textContent`.
- Plots are SVG, built with `h()`-style `createElementNS` helpers, or a 2D canvas (constellation, eye). The WebGL waterfall is the only WebGL user.

**Skeleton build (measured):** see "Skeleton measurements" at the end of this ADR.

## 2. Component tree

```
app.html
├─ TopBar                         shell.ts                               T-150
│   brand · Explore/Decode toggle · device state (live/replay/finished) · recording pill
│   · Centre/Span readouts · connection note · Go to · Review button (open-alarm badge) · Theme
├─ Explore  (#view-explore)
│   ├─ Sidebar
│   │   ├─ Inventory  [slot inventory]  Confirmed | Candidates tabs, counts, tab note,
│   │   │     sortable rows (freq, chips, bandwidth, level/SNR or recurrence dots),
│   │   │     Promote/Delete always reachable (no horizontal scroll)       T-151
│   │   └─ Selections [slot selections] rows (name, extent), focus on click  T-151
│   ├─ Centre
│   │   ├─ LiveView   [slot live]  spectrum trace + WebGL waterfall, brackets (confirmed /
│   │   │     candidate / focused), DC mask, drag-to-select, click-to-focus, hover crosshair
│   │   │     + readout, time scale, zoom/pan, review-mode rendering        T-152
│   │   ├─ Axis       [slot axis]  frequency ticks                           T-152
│   │   └─ Capture    [slot capture] activity band, playhead scrub, LIVE pill,
│   │         "reviewing N ago" note                                         T-150
│   └─ FocusPanel     [slot focus]  signal: state/family/flag chips, big frequency,
│         refined-from-output note, measurements, ranked explanations (suggestions),
│         actions (Listen, Decode, Record/Export, Stream out, Promote/Delete), decoded
│         summary; selection: extent, found-inside list, Listen to all, Export, Watch,
│         Delete                                                             T-151
├─ Decode  (#view-decode)
│   ├─ WorkbenchSide  [slot pipelines] Pipelines (running/hopping), New from recipe,
│   │     Recipe stage list, Blocks palette                                  T-153
│   ├─ StageStrip     [slot stages]  node chain with per-node status         T-153
│   ├─ StagePlots     [slot plots]   two plots for the selected stage        T-153
│   ├─ PacketInspector[slot inspector] frame list → hex+ASCII → layer tree,
│   │     linked selection both ways, served address                         T-154
│   └─ StageParams    [slot params]  step guide, block parameters with blind
│         "Use" suggestions, live quality tiles, Next / Save / Stream / Reset  T-153
├─ OutputsDock        [slot outputs] every stream this page opened plus pipeline outputs:
│     kind, label, meter or rate, Mute/Open, Copy address, Stop               T-150
└─ ReviewDrawer       [slot review]  tabs: Alarms · Report · History · Scheduler ·
      Device · Bookmarks (the M0b–M2 panels, rehomed)                        T-155
```

Mount contract (`ui/src/app/context.ts`): `type MountFn = (el: HTMLElement, ctx: AppContext) => void`, with `ctx = { store, client, token }`.

Rules for panels:
- A panel never queries or changes DOM outside `el`. The one exception is `shell.ts`, which owns the top bar and the view switch.
- Each area's `ui/src/app/<area>/index.ts` exports `mounts: AreaMounts` (slot name → mount). `main.ts` imports the six areas once and mounts every table; a slot appears in exactly one area (tested). The owning task swaps its placeholders in its own `index.ts`; `decode/index.ts` (T-153) takes the `inspector` mount from T-154's `decode/inspector.ts`.
- Shared DOM helpers (`dom.ts`) and transport helpers (`net.ts`) belong to T-149. A change to them is a small, reviewed edit made by T-150.

## 3. State store

`createStore<S>(initial)` has four methods:
- `get()`;
- `set(patch | (s) => patch)`, which merges at the top level, notifies only when a value changed, and queues calls re-entered from a listener;
- `subscribe(fn)`;
- `select(sel, fn, {eq, immediate})`, which fires only when the selected value changes (`Object.is` or `eq`).

A slice that changes must be a new object or array.

### 3.1 Slices

| Slice | Contents | Written by | Read by | Source |
|---|---|---|---|---|
| `mode` | `explore` \| `decode` | shell (toggle); T-151 "Open in Decode"; T-150 dock "Open" | shell, every panel that pauses while hidden | UI; persisted per viewer in `localStorage` (`hk-mui-prefs`) |
| `theme` | `system` \| `dark` \| `light` | shell | shell (stamps `data-theme`) | UI; persisted per viewer |
| `review` | `{open, tab, region}` | shell (button); T-151 (selection History); T-155 | T-155 | UI |
| `conn` | `{api, spectrum, message}` | net and poll error handlers; T-152 | shell (connection note, token dialog) | transport |
| `device` | `{loaded, live, finished, contentClass, centerHz, sampleRateHz, rowsPerS, recording}` | shell poll (T-150 only) | top bar, T-152 (time scale), T-155 (read only; its Device tab keeps the full control state in `review/slice.ts`) | `GET /api/control/state`, 2 s |
| `live` | `{streamId, centerHz, bandwidthHz, bins, rowRateHz, view}` | T-152 (header, zoom and pan) | T-151 (inventory span), T-150 (capture), T-155 (report default span) | spectrum stream header plus UI zoom |
| `focus` | `none` \| `{signal, id}` \| `{selection, id}` | T-151, T-152 (click or drag) | T-151 focus panel, T-152 brackets | UI |
| `inventory` | `{tab, sort, rows by id, loadedAtS, error}` | T-151 poll | T-151, T-152 (brackets), T-150 (dock labels) | `GET /api/inventory`, 5 s, for the view span |
| `selections` | `{list, sync}` | T-151 (mirrors `SelectionStore`) | T-152 (boxes), T-151 | `/api/selections` via `SelectionStore` |
| `outputs` | `OutputEntry[]` | T-150 (`upsertOutput` / `removeOutput`); T-151 and T-153 through T-150's `outputs` actions | T-150 dock, T-151 (on-air dots, Listen pressed) | streams this page opened plus `GET /api/pipelines` |
| `time` | `{live: true}` \| `{live: false, tS}` | T-150 (capture scrub, LIVE) | T-152 (history render), T-151 (inventory `t0`/`t1`), shell | UI cursor over the retained range |
| `decode` | `{pipelineId, nodeId}` | T-153 (pipeline and stage) | T-153, T-154 | UI |
| `inspector` | `{frameSeq, fieldNodeId}` | T-154 (frame and field) | T-154 | UI |
| `nav` | `{gotoHz, seq}` | shell (Go to) | T-152 (pan or retune), T-151 (focus nearest known row) | UI one-shot request |
| `toast` | `{text, seq}` | anyone, via `toast(text)` | shell | UI |

Each slice (type, initial value, pure actions) lives in its owner's file, and `state.ts` only composes them (`AppState extends` every area's state type; `initialState` spreads every area's initial value; it re-exports all slice modules, so `import … from "./state"` keeps working):

| File | Owner | Top-level keys |
|---|---|---|
| `app/shell-slice.ts` | T-150 | `mode`, `theme`, `conn`, `device`, `nav`, `toast` (+ `parsePrefs`, `setMode`, `cycleTheme`, `requestGoto`, `toast`) |
| `app/capture/slice.ts` | T-150 | `time` (+ `goLive`, `reviewAt`) |
| `app/dock/slice.ts` | T-150 | `outputs` (+ `upsertOutput`, `removeOutput`) |
| `app/explore/slice.ts` | T-151 | `focus`, `inventory`, `selections` (+ `focusSignal`, `focusSelection`) |
| `app/centre/slice.ts` | T-152 | `live` |
| `app/decode/slice.ts` | T-153 | `decode` |
| `app/decode/inspector-slice.ts` | T-154 | `inspector` |
| `app/review/slice.ts` | T-155 | `review` (+ `toggleReview`, `openReview`) |

A task adds new top-level keys in its own slice file (e.g. T-155 adds `bookmarks`, and T-152 draws them when the key exists). Writers of another task's key use that owner's exported actions.

### 3.2 How the API feeds the store

- **Polls** (`startPoll`, no overlap, backoff on failure, paused while the owning view is hidden where noted):

  | Route | Interval | Notes |
  |---|---|---|
  | `/api/control/state` | 2 s | always |
  | `/api/inventory?f_lo&f_hi&state=candidate,confirmed` | 5 s | Explore, plus after any action |
  | `/api/selections` | via `SelectionStore` | offline flush every 15 s |
  | `/api/pipelines` | 2 s | Decode, or while the dock shows pipeline outputs |
  | `/api/anomalies?status=open&limit=100` | 30 s | Review badge |
  | `/api/history` (capture band) | 60 s, and on scrub release | |
  | `/api/analysis/strongest` | 1 s | only while Listen has no explicit target, as T-079 does today |

- **Streams** (`net.openStream`; one socket per stream; the owner reconnects with `backoffMs`):
  - **`/ws/spectrum/live`** (found in `/api/streams` by `kind=spectrum && remote_permitted`). The header sets `live` geometry, and rows go straight to `Waterfall.push`. On close after a header, reconnect at once; otherwise back off and rediscover.
  - **`/ws/open/listen?emitter=|f_lo&f_hi`**, one per dock audio entry:
    - a refusal (`{type: "refused"}`) sets the entry to `refused` with its reason;
    - status records (type 3, `level_dbfs`, `snr_db`, `squelch_open`, `refined_center_hz`) update the entry at ≤ 4 Hz;
    - PCM goes to the worklet;
    - no automatic reconnect: a closed listen becomes `ended`, with Retry in the dock.
  - **`/ws/open/inspector?pipeline=<id>`**, opened only through T-153's `decode/status-feed.ts` `subscribePipelineFeed` (one reference-counted socket per pipeline). T-154 subscribes to frames and keeps them in a panel-local ring of 200. T-153 subscribes to status records (`<node>.lock/quality/error_rate`) and summarises them into its quality tiles at ≤ 4 Hz. On close, the feed reconnects with backoff while the pipeline is `running`.
  - **`/ws/open/stage?pipeline&node&port[&view=spectrum]`** (T-153): opened only for the visible stage's plots and closed on stage change (a tap costs nothing until opened).
  - **Optional `/ws/anomalies`** (T-155) replaces the 30 s poll when present in `/api/streams`.
- **Errors:**
  - 401 sets `conn.api = unauthorized`; the shell shows the token dialog, and reload resumes.
  - A network failure or 5xx sets `offline`; polls back off, and the top bar shows "server unreachable".
  - `ControlError.code` messages go to the owning panel's inline status (for example `busy` at the chain budget, `refused`, `not_live`, `outside_window`), never a global modal.
- **High-rate data never enters the store:** spectrum rows, PCM, frame bytes beyond the visible frame, and per-frame meter samples.

### 3.3 Time cursor semantics (LIVE vs reviewing)

- **`time.live`:** the waterfall renders the live stream, and the inventory is unbounded in time.
- **Reviewing (`{live: false, tS}`):** the stream stays connected, but rows aren't pushed.
  - T-152 renders `GET /api/history?f_lo&f_hi&t0=tS−window&t1=tS&format=json` (`max_db`, unobserved cells drawn grey), into the same `Waterfall` via `push` after a reset.
  - T-151 adds `t0`/`t1` to inventory queries.
  - Outputs keep playing live.
  - The Go to action and device controls still act on the live device.

## 4. Frontend↔API map

Update modes: **poll** (interval in §3.2), **stream**, **action** (on user action), **header** (stream header). Rows marked **API GAP n** are listed in §4.9.

### 4.1 Top bar

| Element | Route / stream | Fields used | Mode | Note |
|---|---|---|---|---|
| Explore/Decode toggle | – | – | – | UI state `mode` |
| Device "HackRF One · live" | `GET /api/control/state` | `live`, `device` (caps name when present), `run.finished` | poll | replay shows "replay" |
| Recording pill "Recording all · 48 h buffer" | `GET /api/control/state` | `run.recording.active` | poll | **API GAP 1**: no always-on buffer status. Interim: the pill shows only a manual recording in progress. |
| Centre / Span readouts | `GET /api/control/state` | `tuning.center_hz` or `run.center_hz`; `tuning.sample_rate_hz` or `run.sample_rate_hz` | poll | |
| Go to | `POST /api/control/center` (outside the tuned window, live only) | `center_hz` | action | Inside the window, T-152 pans and T-151 focuses the nearest *already loaded* inventory row, which is lookup over known UI state, not analysis. Replay answers `409 not_live`, shown as a toast. |
| Review badge | `GET /api/anomalies?status=open&limit=100` | `anomalies.length` | poll | "99+" past the page |
| Theme | – | – | – | UI |

### 4.2 Explore: inventory and selections (left sidebar)

| Element | Route / stream | Fields used | Mode | Note |
|---|---|---|---|---|
| Confirmed / Candidates tabs and counts | `GET /api/inventory?f_lo&f_hi&state=confirmed` / `state=candidate` (`cursor`, `limit` ≤ 500) | `entries[].state`, `next_cursor` | poll | counts are rows in the view span ("this span"). **API GAP 13**: no `total`, so a count caps at the page size. Interim: show "500+" when `next_cursor` is present. |
| Row frequency, bandwidth | same | `f_center_hz` (or `refined.center_hz` when present), `bandwidth_hz` | poll | |
| Family / flag chips | same | `family`, `classification.family`, `explanations[0].flags` (`off-raster`, …), `known_status` | poll | "unknown" when `family` is null |
| Candidate recurrence dots, "14×/h" | same | `recurrence.recent[]`, `recurrence.occurrences`, `span_s`, `duty_cycle` | poll | dots are a sparkline of `recent[].count` |
| Confirmed level bar and SNR | same | – | – | **API GAP 2** (no `snr_db`/`peak_dbfs` on the row). Interim: show `recurrence.duty_cycle` and `count`. |
| On-air dot | – | `outputs[]` with matching `emitterId` | store | |
| Sort | – | client-side `sortRows` over loaded rows | – | ordering known values is presentation |
| Promote | `POST /api/inventory/{id}/promote` | `{changed, entry}` | action | then re-poll |
| Delete | `DELETE /api/inventory/{id}` | `{deleted}` | action | |
| Selections list | `GET/POST/PUT/DELETE /api/selections[/{id}]` | `id, name, f_lo, f_hi, t_lo, t_hi, links` | poll / action | through `SelectionStore` (offline queue) |

### 4.3 Explore: centre

| Element | Route / stream | Fields used | Mode | Note |
|---|---|---|---|---|
| Spectrum trace + waterfall | `/api/streams` → `/ws/spectrum/live` | header `center_hz, bandwidth_hz, fft_size, sample_rate_hz, content_class`; records type 1/2 | stream | existing `Waterfall` (skeleton wired) |
| Brackets (confirmed / candidate / focused) | inventory slice | `f_lo_hz, f_hi_hz, state` | store | |
| DC mask | `GET /api/observations?f_lo&f_hi&t0&t1&tier=interactive&limit=1` | `records[].window.dc_excluded` | poll (on retune) | **API GAP 10**: not on the spectrum header or control state, and absent without an observation log. Interim: no mask when unavailable. |
| Drag to select | `POST /api/selections` | `name, f_lo, f_hi, t_lo?, t_hi?` | action | pixel↔Hz via `axis.ts` |
| Click to focus | inventory slice | nearest row by extent (`inspect.nearestEntry`) | store | |
| Hover readout | waterfall newest row | `Waterfall.levelAt`, `axis.snapHz` | – | |
| Time scale "↓ 20 s" | `device.rowsPerS` × `Waterfall.rows` | | poll | |
| Axis ticks | `axis.ticks` | | – | |
| Review render | `GET /api/history?…&format=json` | `max_db`, `nf`, `nt`, `f_lo_hz`, `f_cell_hz`, `t0_s`, `t_cell_s`, `coverage` | action (scrub) | unobserved cells are not quiet: draw grey |
| Markers / bookmarks | `GET /api/bookmarks` | `f_center_hz, name, kind` | poll (T-155 slice) | |

### 4.4 Capture timeline (T-150)

| Element | Route / stream | Fields used | Mode | Note |
|---|---|---|---|---|
| Activity band | `GET /api/history?f_lo&f_hi&t0=now−48h&t1=now&max_cells≈192×nf` | per time column: max of `occupancy` / `max_db` over the span; `coverage_summary.gaps` | poll 60 s | The reduction per column is a max over served cells, for display. Gaps are drawn as unobserved. |
| Playhead scrub / LIVE pill / "reviewing N ago" | – | `time` slice | – | UI |
| "6.2 h buffered · 41 GB of 220 GB · last 48 h kept" | – | – | – | **API GAP 1**. Interim: show history coverage span (`coverage_summary`) as "history since …". *Served since T-157/T-178:* `GET /api/iqbuffer` `span_s`, `bytes`, `allocated_bytes`, `retention_s`; the buffer is a pre-allocated ring that survives restarts ([ADR-0014](0014-iq-capture-ring.md)), so the span can reach back before the current run (segments carry `run`). |
| "Export clip from the buffer" | `POST /api/outputs/record/start {selection_id\|emitter_id\|band, kinds:["iq"]}` | `recording` session | action | **API GAP 1** (records forward, not from the buffer). Interim action label: "Record IQ". *Served since T-157:* `POST /api/iqbuffer/clip` (`run` selects a segment of an earlier run, T-178). |

### 4.5 Explore: focus panel (T-151)

| Element | Route / stream | Fields used | Mode | Note |
|---|---|---|---|---|
| State and family chips | `GET /api/inventory/{id}` | `state, family, classification, known_status, explanations[0].flags` | poll 5 s while focused | |
| Big frequency | same | `refined.center_hz` else `f_center_hz` | | |
| Refined-from-output note | same | `refined` (`provenance`, `objective`, `mode`, `start_center_hz`, `converged`, `t_s`), or `null` | | "Centre from detection; not refined yet" when null |
| Bandwidth | same | `refined.bandwidth_hz` else `bandwidth_hz` | | |
| SNR, peak level | – | – | – | **API GAP 2** |
| Seen | same | `recurrence.span_s, on_air_s, duty_cycle, occurrences`, `first_seen_s`, `last_seen_s` | | |
| Channel raster | same | `explanations[i].evidence[]` of kind `Raster` (`offset_hz`, `on_raster`, `raster_hz`) | | |
| Ranked explanations | same | `explanations[]`: `rank, label, score, flags, status, evidence[]` (`BandPlan.reason`, `Family.confidence`, raster offset) | | "why" text is formatted from evidence; always labelled "suggestions" |
| Decoded summary (RDS PI/PTY/PS) | same | `identity_scheme`, `identity_value`, `withheld` | | **API GAP 3**: only the identity is on the row, not the latest decoded fields |
| Listen / Stop listening | `/ws/open/listen?emitter=<id>` | header `audio.mode`, `refinement`; status records | stream | adds or removes a dock entry (T-150 actions) |
| Decode / Open in Decode | `GET /api/recipes` → `POST /api/pipelines {recipe_id, target: {emitter_id}}` | `recipes[].match`, pipeline `id` | action | Recipe ranking for this emitter is **API GAP 7b**. Interim: list all recipes; `match` hints are shown, never auto-started. |
| Record / Export clip | `POST /api/outputs/record/start {emitter_id, kinds}` | session | action | see GAP 1 |
| Stream out (audio) | `/api/streams` `tcp.addr` + `open/listen?emitter=<id>` | | action | copy address |
| Stream out (IQ) | – | – | – | **API GAP 8** |
| Promote / Delete | as §4.2 | | action | |
| Selection: extent, found inside | `GET /api/inventory?f_lo&f_hi` | rows | poll | order by `count`, or by level once GAP 2 lands |
| Listen to all | N × `/ws/open/listen?emitter=` | | stream | a `4503` refusal at the budget is shown per entry |
| Selection History | – | `review` slice → T-155 History tab | store | |
| Watch | – | – | – | **API GAP 9**. Interim: hidden. |
| Delete selection | `DELETE /api/selections/{id}` | | action | |

### 4.6 Decode: workbench (T-153)

| Element | Route / stream | Fields used | Mode | Note |
|---|---|---|---|---|
| Pipelines list, running / hopping | `GET /api/pipelines` | `id, recipe_id, state, end_reason, channel, follow_hops, stats, status` | poll 2 s | `follow_hops` is served but undocumented (**API GAP 11**) |
| New from recipe (RDS / POCSAG / ACARS / ADS-B) | `GET /api/recipes` (builtin), `POST /api/pipelines` | `id, name, builtin, match` | action | target = focused emitter, or selection, or view band. Refusals (`outside_window`, `busy`, `unrealisable`) shown inline. |
| Recipe stage list, stage strip | `GET /api/recipes/{id}/versions/{version}` + pipeline `nodes[]` | node `id, block, params`; `status["<node>.lock"]` etc. | poll | chip text from status tokens |
| Blocks palette | `GET /api/blocks` | `name, group, doc, inputs, outputs, params` | once | drag → draft recipe |
| Hot edit (add block, change param) | `POST /api/recipes/validate` then `PUT /api/pipelines/{id}/recipe` | `valid, errors[].path`; `plan`, `swap` | action (debounced 300 ms) | never stops capture |
| Save recipe | `POST /api/pipelines/{id}/save` | `version` | action | |
| Channel map | `PUT /api/pipelines/{id}/channels`, `POST …/channels/refresh`; frames `metadata.channel` | | action / stream | |
| MPX / subcarrier plot | `/ws/open/stage?pipeline&node&port&view=spectrum` | `rf32_le` rows | stream | **API GAP 4** (`view=spectrum` answers 422) |
| Constellation | `/ws/open/stage?…&port=<soft\|iq port>` | `symbols`/`iq` payload | stream | scatter of served values, decimated for drawing |
| Eye / timing diagram | – | – | – | **API GAP 5**. Interim: bits-and-soft strip from `open/stage` on the `soft` and `bits` ports. |
| Sync-search plot | – | – | – | **API GAP 6**. Interim: `status["<sync node>.lock/quality"]` tile. |
| Group / frame counts by type or channel | inspector stream frame records | `metadata.<mapped key>`, `metadata.channel` | stream | tally of already-decoded values over 60 s (presentation) |
| Step guide text | `GET /api/blocks` `doc`; recipe `description` | | once | **API GAP 12** (per-node prose) |
| Parameters | recipe node `params` + `/api/blocks` `params` (`type, default, hot, doc`) | | poll / action | |
| Blind "Use" suggestions | `POST /api/assist/sync`, `/api/assist/crc`, `/api/assist/fields` (frames from `GET /api/captures/{id}/frames`) | `syncs[].fragment`, `codes[].fragment`, `score`, `reasons` | action | Symbol-rate / subcarrier suggestions for an emitter: **API GAP 7a** |
| Live quality tiles (CRC/BCH %, records/s, lock) | inspector `status` records; `GET /api/pipelines` `stats.frames` | `<node>.error_rate, quality, lock, blocks_ok`; `frames` delta ÷ Δt | stream / poll | |
| Stream records | `/api/streams` (`tcp.addr`, `tcp_target` of `inspector/<p>/<o>`) | | action | adds a dock entry |

### 4.7 Decode: packet inspector (T-154)

| Element | Route / stream | Fields used | Mode | Note |
|---|---|---|---|---|
| Output stream frame list | `/ws/open/inspector?pipeline=<id>` | frame records `seq, t, metadata.{frame, channel, fit, crc…}`, `content.hex`, `gated` | stream | ring of 200; gated frames show "withheld" |
| Past frames / scrub | `GET /api/captures?…`, `GET /api/captures/{id}/frames?from_t&limit` | `frames[]`, `next_from_frame` | action | reuse `frame-inspector.ts` helpers |
| Hex + ASCII | frame record | `content.hex` (`hexBytes`, `asciiChar`) | – | byte colour from `layers.nodes[].bytes` |
| Layer tree | frame record | `content.layers.nodes` (`buildTree`), `errors` | – | |
| Linked selection | – | `byte_index[b]` (`cycleLeafAt`), node `bytes` | – | exactly the api.md "Linked selection uses only these" rule |
| Re-parse with draft field map | `POST /api/captures/{id}/parse` | `frames`, `fit` | action | |
| Served address | `/api/streams` | `tcp.addr`, stream `tcp_target` | once | |

### 4.8 Outputs dock (T-150) and Review drawer (T-155)

| Element | Route / stream | Fields used | Mode | Note |
|---|---|---|---|---|
| Audio entry, meter | `/ws/open/listen` | status `level_dbfs`, `squelch_open`, `snr_db` | stream | one shared `AudioContext`; per-entry `GainNode` for Mute |
| Records entry, rate | `GET /api/pipelines` | `stats.frames` delta, `outputs[]` | poll | |
| Copy address | `GET /api/streams` | `tcp.addr`, `tcp_target` / on-demand `tcp_target` + params | once | the token is never copied |
| Stop | close socket / `DELETE /api/pipelines/{id}` | | action | |
| Open | – | `mode=decode`, `decode.pipelineId` | store | |
| Recordings (manual) | `GET /api/outputs`, `POST /api/outputs/record/stop`, file URLs + `?token=` | `recordings[]` | poll 2 s while active | a download link is a GET, so `?token=` is allowed |
| Alarms tab | `GET /api/anomalies`, `/{id}`, `POST …/dismiss`, `…/reopen` | as `alarms.ts` | poll / action | |
| Report tab | `GET /api/report?f_lo&f_hi&t0&t1[&format]` | as `report.ts` | action | default region = `live.view`, span = the capture timeline range |
| History tab | `GET /api/history`, `GET /api/floor` | as `history.ts` | action | opened from a selection on its region |
| Scheduler tab | `GET /api/scheduler`, `/arms`, `/leases`; `POST/DELETE /api/scheduler/leases` | as `scheduler.ts` | poll / action | |
| Device tab | `/api/control/*` | `controls/model.ts` `panelModel` | poll / action | gains, span, bias tee, baseband filter, display, pause |
| Bookmarks tab | `/api/bookmarks*` | `controls/bookmarks.ts` | poll / action | |

### 4.9 API gaps and proposed backend tasks

None are implemented here. Each gap has an interim UI behaviour, and none of them computes the missing measurement in the browser.

| # | Gap (mockup element) | Proposed backend task | Owning crate(s) | Size |
|---|---|---|---|---|
| 1 | Always-on capture buffer: "Recording all · 48 h buffer", buffered hours and bytes of quota, "Export clip from the buffer" | **Rolling IQ capture buffer with status and clip export**: retention policy, `GET /api/capture/buffer` (`retained_from_s`, `bytes`, `quota_bytes`), `POST /api/capture/clip {t0, t1, f_lo, f_hi}` → SigMF output session | hk-store, hk-pipeline, hk-api | large — **done**: T-157 `GET /api/iqbuffer`, `POST /api/iqbuffer/clip`; T-178 persistent pre-allocated ring ([ADR-0014](0014-iq-capture-ring.md)) |
| 2 | Row and focus SNR, peak level, level bar | **Inventory row measurements**: latest `snr_db`, `peak_dbfs` (and `floor_dbfs`) from the emitter's detections or track | hk-model (repo query), hk-api | small — **done**: T-158 |
| 3 | Focus decoded summary (RDS PS/PTY, pager address) | **Emitter's latest decode fields**: `GET /api/inventory/{id}/decodes?limit=` returning Decode rows gated like the `decodes/*` stream | hk-model, hk-api | small — **done**: T-159 (served as `GET /api/inventory/{id}/decode`) |
| 4 | MPX / subcarrier stage plot | **Stage tap `view=spectrum`** (already specified in stream-contract §14.4; answers 422) | hk-pipeline (recipes/openers), hk-api | small — **done**: T-160 |
| 5 | Eye diagram, timing diagram | **Clock-recovery diagnostic output**: per-symbol waveform segments and sample instants on a diagnostic port, or `view=eye`; stream-contract §14.4 addition | hk-pipeline (blocks), hk-stream docs | large |
| 6 | Sync-search plot | **`sync_search` diagnostic port**: match score per candidate position | hk-pipeline (blocks) | small — **done**: T-162 (stage tap `view=sync_search`, §4.3 row above) |
| 7a | Blind "Use" suggestions for symbol rate, subcarrier, deviation | **Emitter estimated parameters on inventory**: latest `EstimatedParams` (symbol rate, modulation, deviation, CFO, bandwidth) on `GET /api/inventory/{id}` | hk-model, hk-api | small |
| 7b | "Decode RDS" / recipe choice for a signal | **Serve `GET /api/recipes/match?emitter=`** (planned in api.md, T-088): recipes ranked against measured parameters, with reasons | hk-pipeline (recipes), hk-api | large |
| 8 | Stream out IQ | **On-demand channelised IQ opener `open/iq?emitter\|f_lo&f_hi`** (§12.1 profile, `cf32_le`) | hk-pipeline (chains), hk-api | small |
| 9 | Selection "Watch: alert on new activity" | **Region watch**: a selection-scoped alarm rule raising anomalies and stream messages for new activity in its extent | hk-context, hk-api | large |
| 10 | DC mask on the live view | **DC notch extent on the live geometry**: `dc_excluded_hz` on the spectrum stream header (and `run`) | hk-pipeline, hk-api, stream-contract | small — **done**: T-167 |
| 11 | Hopping pipelines | **Document pipeline `follow_hops`** (served by `recipes/runtime.rs`) in docs/api.md, with a contract assertion | hk-api docs, hk-cli tests | small |
| 12 | Decode step guide prose | **Recipe per-node `doc`** (additive recipe-schema field, shown in the step guide) | hk-recipe | small |
| 13 | Inventory tab counts past one page | **Inventory `total`**: the matching row count (before `limit`/`cursor`) on `GET /api/inventory`, with a contract assertion | hk-model (repo query), hk-api | small — **done**: T-171 |

Small means ≤ 1 day for one agent with contract tests; large means a new store, block or contract surface needing review. GAPS 4, 6, 7a, 10 and 11 unlock most of the mockup and are independent of each other. GAP 5 changes the stream contract, so it goes to Fable or Opus.

## 5. Responsive and theme rules

- **Breakpoints** (from the mockup): three columns above 1150 px (sidebar 290 / centre / focus 330; Decode 300 / centre / 330); narrower columns at ≤ 1150 px (250 / 290), with optional readouts hidden.
- **At ≤ 900 px**, one column:
  - the page scrolls vertically, but never horizontally (`body { overflow-x: hidden }`);
  - the top bar wraps;
  - the sidebar gets fixed heights (inventory 240, selections 130), the centre 370 + 26 + 92, and Decode 440 / 460;
  - the dock wraps and the Review drawer goes full-screen.
- **No horizontal scroll** except inside components that are inherently wide: the stage strip (`.pipe`), the hex grid, the dock strip. Each gets its own `overflow-x: auto`.
- **No fixed `min-width`** wider than 400 px. Row actions must stay reachable without horizontal scroll (the T-148 rule): action buttons wrap below the row at narrow widths, and on touch they are always visible, not hover-only.
- **Touch:** `touch-action: none` on the live canvas only. Tap = click. A drag with ≥ 6 px of travel selects; the old UI's `DRAG_PX` rule is kept.
- **Theme:** tokens live on `:root`, dark by default. Light tokens apply under `prefers-color-scheme: light` unless `data-theme="dark"`, and under `data-theme="light"`. Rules:
  - components use tokens only;
  - data colours stay token-derived (teal = confirmed, lavender = candidate, amber = focus/selection, coral = flags);
  - the waterfall colormap stays fixed because it's data, but its surrounding chrome follows the theme.
- **Fonts:** system stacks only (CSP).
- **Accessibility:** `:focus-visible` outline; rows are focusable with Enter/Space; `aria-pressed` on toggles; `aria-current` on the focused row, stage and frame; `prefers-reduced-motion` disables transitions.

## 6. Testing

- **Unit tests (existing style):**
  - `node:test` files in `ui/test/*.test.ts`, bundled by esbuild and run under `node --test` by `ui/test/run.mjs` (`npm test`, `just test-ui`). The runner globs every `*.test.ts`, so a new `app-<panel>.test.ts` is picked up without editing `package.json`; `npm test -- app-store` runs only matching files;
  - each panel keeps its logic in pure exported functions (query builders, row→view-model, action→API call with an injected client, formatting), tested with fixture JSON captured from `hk serve --replay`;
  - layout checks read `src/app/index.html` and the CSS files `app.css` imports as text, as the skeleton's `app-shell.test.ts` does.
- **Store tests:** every action in `state.ts` and the store contract (`app-store.test.ts`).
- **Contract reliance:** the UI trusts `docs/api.md` as enforced by `crates/hk-cli/tests/api_contract.rs` (T-079). A UI task that needs a new field gets it from an API GAP task that updates api.md and the contract test together; the UI never guesses a shape.
- **Fixtures:** `/api/*` JSON fixtures live under `ui/test/fixtures/` (T-156 moves the existing ones there), each with a `_comment` naming the command that produced it.
- **DOM harness (optional, T-156):** at most one dev dependency (a DOM shim such as `linkedom`), only if mount-level tests pay for themselves. Canvas, WebGL and audio are never tested headless; they're checked manually on the demo (desktop and a ~400 px viewport).
- **Acceptance per task:** `npm run build && npm run typecheck && npm test` green, plus the task's checks in §8.

## 7. Migration plan

1. **T-149 (this):** skeleton at `/app.html` (`ui/src/app/`); `/` unchanged. Both pages share the `hk-token` session key, so `http://…/app.html#token=…` works.
2. **T-150…T-155 in parallel:** each replaces its placeholders in its own area `index.ts` and owns its files (§8). They import the old pure helpers (`inventory.ts`, `selections.ts`, `frame-inspector.ts`, `alarms.ts`, `report.ts`, `scheduler.ts`, `controls/*`, `axis.ts`, `audio-frames.ts`, `jitter.ts`, `waterfall.ts`) and must not change their exported behaviour, because the old page still uses them. If one needs a change, the task adds a new function rather than altering an old one. Every merge keeps both pages building and green, so the demo stays mergeable.
3. **T-156 (done, 2026-09-15):**
   - `src/app/index.html` now builds straight to `dist/index.html` (and is copied again to `dist/app.html` as an alias — no `dist/classic.html` step was needed, since nothing outside `ui/src/app/main.ts`'s own comment and this ADR referenced `/app.html`);
   - deleted the DOM-bound old classes and their files: `main.ts` (`Live` and all DOM wiring), `listen.ts` (`Listener`, `installListen`), `selection-panel.ts` (`SelectionPanel`), `controls/panel.ts` (`ControlPanel`), and the old `index.html`/`style.css`; trimmed `InventoryTable`, `FrameInspectorPanel`, `ReportPanel`, `AlarmsPanel`, `SchedulerPanel`, `HistoryPanel` and `BookmarkPanel` out of their files, keeping every pure helper the MUI app or its tests still use, in place (no files moved to `ui/src/app/lib/` — the flat `ui/src/*.ts` layout the panel tasks already imported from was kept to minimise churn);
   - replaced the CSS-hardcoded dark-theme `rgba(...)` overlays in `capture.css`, `centre.css`, `decode.css`, `decode/inspector.css`, `dock.css` and `explore.css` with `color-mix(in srgb, var(--token) N%, transparent)`, so they follow the light/dark tokens instead of always being dark-theme RGB values;
   - added a narrow-width CSS/layout test per area (`app-decode.test.ts` for `decode.css`+`inspector.css`, `app-dock.test.ts`, `app-capture.test.ts`, `app-review.test.ts`, plus a sidebar-structure check in `app-explore.test.ts`), alongside the T-151/T-152 ones that already existed;
   - deleted the old `index.html`-reading layout tests in `inventory.test.ts`, `frame-inspector.test.ts`, `report.test.ts`, `alarms.test.ts`, `scheduler.test.ts` (kept every pure-function test in those files);
   - did **not** move the `/api/*` JSON fixtures (`control_state_replay.json`, `spectrum_axis.golden.json`) into `ui/test/fixtures/`: `spectrum_axis.golden.json`'s path is hardcoded in `tests/e2e/tests/spectrum_axis.rs` (`HK_UPDATE_GOLDEN=1`), and moving just one of the two seemed worse than moving neither — left as a follow-up;
   - `dist/app.js` is 140.2 KB minified / 48.2 KB gzip (budget ≤150/≤45 KB) — over the gzip budget by about 3 KB after T-150–155's real feature set landed on top of the T-149 skeleton measurement; `dist/app.css` is 32.3 KB (budget ≤40 KB, no gzip figure given), comfortably under. Flagged for the coordinator rather than cut: shrinking further means trimming shipped functionality, not dead code (tree-shaking already removes the pure helpers T-156 kept for tests, e.g. `nextSort`/`sortRows`/`loadInventoryPage`/`bookmarkFromClick`/`jumpPlan` do not appear in the built bundle).
   - rewrote `ui/README.md` to describe the MUI app (it previously documented only the retired
     single-page UI, predating even T-149).

4. **T-179 (2026-09-15):** brought `dist/app.js` back under the gzip budget by code-splitting the two rarely-used areas instead of cutting features. `npm run build` now bundles `src/app/main.ts` with esbuild `--splitting --format=esm` (`index.html`'s script tag is `type="module"`); `decode/index.ts` and `review/index.ts` are loaded with a dynamic `import()` the first time `mode` becomes `"decode"` or `review.open` becomes true (`main.ts`), instead of the previous static import of every area. Measured: entry `dist/app.js` 60.2 KB minified / 21.6 KB gzip, plus three small shared chunks it still imports eagerly (`dom.ts`, `net.ts`, `state.ts`, `controls/{client,freq,model}.ts`, the slice files) at 17.9 KB minified / 7.7 KB gzip combined — **initial load 78.1 KB minified / 29.3 KB gzip total, within the ≤150/≤45 KB budget**. The two lazy chunks (`chunks/decode-*.js` 28.3 KB min / 10.0 KB gzip, `chunks/review-*.js` 35.6 KB min / 11.3 KB gzip) load only on first switch to Decode or first Review-drawer open. `dist/app.css` is unchanged at 32.3 KB. No change was needed in `crates/hk-api/src/http.rs`'s `static_file`: it already serves any relative path under `ui_dist`, chunk filenames use only `[A-Za-z0-9._-]` segments, and `.js` is already served as `text/javascript`.

   The supervisor rebuilds the demo on each UI merge.

## 8. Per-panel implementation briefs (T-150…T-155)

Shared rules for every task:

- **Thin client only.** A new "signal-looking" number must come from a route field. If it's missing, use the §4.9 interim behaviour and name the gap in the task report.
- **Stay inside your subtree.** Mount only into your slots (in your area's `index.ts`), extend only your own slice file (§3.1), and add CSS only in your own area CSS file (`<area>/<area>.css`, or `decode/inspector.css` for T-154), scoped under your slot's class.
- **Panel tasks don't edit `main.ts`, `state.ts`, `app.css` or `package.json`.** Those are composition points that already import every area; a need to change one is a T-149 follow-up, not a panel edit.
- **Leave shared files alone.** Don't edit another task's files, or `store.ts`, `net.ts`, `dom.ts`, `context.ts`, `placeholder.ts`. The only exception is T-150, which may make small additive changes to `net.ts`, `dom.ts` and the shell sections of `base.css`.
- **Cross-task calls go through the stub contracts,** which exist now with typed signatures and doc comments naming the owner. Callers code against them in parallel; the owner implements the bodies without changing the signatures (a signature change is a reviewed edit coordinated with the callers):
  - `app/dock/api.ts` (owner T-150; callers T-151, T-153). `startListen(ctx, target: ListenTarget): string | null` with `ListenTarget = {kind: "emitter", emitterId, label} | {kind: "band", fLoHz, fHiHz, label}`; `startRecordsOutput(ctx, target: {pipelineId, outputId, label}): string | null`; `stopOutput(ctx, id): void`. Until T-150 lands, the start functions toast "not implemented yet (T-150)" and return null; `stopOutput` is a no-op.
  - `app/decode/status-feed.ts` (owner T-153; caller T-154). `subscribePipelineFeed(ctx, pipelineId, {status?(u: StatusUpdate), frame?(f: FrameRecord), state?(s: FeedState, message)}): () => void`, one reference-counted `/ws/open/inspector` socket per pipeline (§3.2). Until T-153 lands, it delivers nothing and returns a no-op unsubscribe.
- **Don't change old modules' exports** (§7).
- **Tests:** add `ui/test/app-<panel>.test.ts`; the runner (`ui/test/run.mjs`) picks it up by name.
- **Done when:** build, typecheck and tests are green; the layout matches the mockup at 1440, 1024 and 400 px; there's no page-wide horizontal scroll; both themes are legible; the old `/` page still works.

### T-150: shell, Outputs dock, Capture timeline (Sonnet, medium)
- **Owns:** `ui/src/app/shell.ts`, `shell-slice.ts`, the shell sections of `base.css`, `ui/src/app/dock/*` (`index.ts`, `slice.ts`, `dock.css`, `api.ts`; new: `outputs.ts`, `audio-session.ts`), `ui/src/app/capture/*` (`index.ts`, `slice.ts`, `capture.css`; new: `timeline.ts`); small additive edits to `net.ts` and `dom.ts`.
- **State:**
  - writes `device` (sole writer), `conn.api`, `outputs` (it owns the `upsertOutput`/`removeOutput` actions and implements the `dock/api.ts` contract, `startListen`, `startRecordsOutput` and `stopOutput`, for T-151 and T-153), and `time`;
  - reads `live`, `inventory` (labels).
- **API:**
  - `/api/control/state` (poll), `/api/streams` (tcp address);
  - `/ws/open/listen` per audio entry: a new multi-session `AudioSession` that reuses `audio-frames.ts`, `jitter.ts` and `audio-worklet.js`, with one shared `AudioContext`, gain per entry, and start synchronously inside the click handler for mobile unlock;
  - `/api/pipelines` (records rate), `/api/outputs` (manual recordings), `/api/history` (activity band), `/api/anomalies?status=open` (Review badge count).
- **Acceptance:**
  - Listen *adds* an entry and the same emitter twice doesn't duplicate; Stop closes the socket; Mute is local;
  - Copy address writes `tcp://<addr> <tcp_target>?…` without the token;
  - a refusal shows its reason;
  - scrubbing sets `time` and LIVE restores it; the band draws unobserved as grey;
  - the rec pill follows `run.recording.active` (GAP 1 interim);
  - unit tests cover the dock view model, address formatting, scrub pct→time mapping and the activity-band column reduction.

### T-151: Explore sidebar and focus panel (Sonnet, medium)
- **Owns:** `ui/src/app/explore/*`: `index.ts`, `slice.ts`, `explore.css`; new `inventory.ts`, `selections.ts`, `focus.ts`, `format.ts`.
- **Tab counts:** "500+" when the page has `next_cursor` (GAP 13 interim).
- **State:** writes `inventory`, `selections` (mirror of `SelectionStore`), `focus`, `review` (History action), `mode` (Open in Decode); reads `live.view`, `time`, `outputs`, `nav`.
- **API:** `/api/inventory` (per tab, view span, `t0`/`t1` when reviewing), `/api/inventory/{id}` (+ promote, delete), `/api/selections*` via `SelectionStore`, `/api/recipes` + `POST /api/pipelines` (Decode action), `POST /api/outputs/record/start` (Record IQ), `dock/api.ts` `startListen`/`stopOutput` (T-150 contract).
- **Acceptance:**
  - Promote and Delete are reachable at 400 px without horizontal scroll, and always visible on touch;
  - the tabs switch to a focused row's state;
  - sort by frequency, last seen and count;
  - the focus panel shows the refined note from `refined`, raster offset from explanation evidence, and explanations labelled as suggestions with scores;
  - GAP 2/3 fields are absent, not invented;
  - Listen toggles the dock entry;
  - Go to focuses the nearest loaded row;
  - unit tests cover the row view model (chips, recurrence text, dots), the explanation "why" formatting from evidence, the refined note, selection found-inside ordering, and action→API calls with a fake client.

### T-152: centre live view (Opus, medium)
- **Owns:** `ui/src/app/centre/*` (`index.ts`, `slice.ts`, `centre.css`, `live-spectrum.ts` from the skeleton, plus new `overlays.ts`, `axis-view.ts`, `review-render.ts`); `ui/src/waterfall.ts` for additive options only (e.g. `setSpecFrac`, `reset()`, theme-neutral line colour), never breaking the old page.
- **State:** writes `live` (geometry, view), `focus` (click), `conn.spectrum`, selections via `SelectionStore` (drag); reads `inventory`, `selections`, `time`, `nav`, `device`, bookmarks (if present).
- **API:** `/api/streams`, `/ws/<spectrum>`, `/api/history` (review render), `/api/observations` (DC mask interim), `POST /api/control/center` (Go to outside the window, live only), `POST /api/selections`.
- **Acceptance:**
  - brackets for confirmed (solid teal), candidate (dashed lavender) and focused (amber), with labels hidden when narrow;
  - selection boxes; drag ≥ 6 px creates a selection, a click focuses the nearest row;
  - hover crosshair + readout;
  - wheel/pinch zoom and pan via existing `controls/gestures.ts` and `axis.ts`;
  - reviewing draws the history grid (unobserved grey) and LIVE resumes;
  - 60 fps at 16k bins not regressed (fps counter checked manually);
  - unit tests cover bracket placement (Hz→%, clamping, narrow rule), history grid→rows mapping, and the goto decision (pan vs retune vs `not_live`).

### T-153: Decode workbench (Sonnet, medium; stage-plot data paths reviewed by Opus)
- **Owns:** `ui/src/app/decode/index.ts`, `decode/slice.ts`, `decode/decode.css`, `decode/status-feed.ts` (contract stub, implement it); new `decode/pipelines.ts`, `stages.ts`, `plots.ts`, `params.ts`, `svg.ts`. Not `decode/inspector*` (T-154).
- **State:** writes `decode` (`pipelineId`, `nodeId`), T-150's outputs via `dock/api.ts` `startRecordsOutput`/`stopOutput`; reads `focus`, `inventory`, `live.view`.
- **API:**
  - `/api/pipelines` (poll + CRUD, recipe PUT, save, channels);
  - `/api/recipes*`, `/api/recipes/validate`, `/api/blocks`;
  - `/ws/open/stage` (visible plots only);
  - `/api/assist/*` + `/api/captures/{id}/frames` (Use suggestions);
  - inspector `status` records (quality) via `decode/status-feed.ts` `subscribePipelineFeed`: T-153 implements it, T-154 subscribes to frames.
- **Acceptance:**
  - pipelines show running/hopping/ended with `end_reason`;
  - selecting a stage opens exactly its taps and closes the previous ones;
  - a parameter change validates then hot-edits, and errors show at the parameter's `path`;
  - Save creates a version;
  - quality tiles come from status keys and the frames rate;
  - GAP 4/5/6 plots show their interim views, labelled "not served yet";
  - unit tests cover the recipe↔stage view model, parameter edit → draft recipe document, status→chip mapping, and frames-rate arithmetic.

### T-154: packet inspector (Sonnet, medium)
- **Owns:** `ui/src/app/decode/inspector.ts` (exports `mountInspector`, imported by T-153's `decode/index.ts`), `decode/inspector-slice.ts`, `decode/inspector.css`; new `decode/hexview.ts`.
- **State:** writes `inspector` (`frameSeq`, `fieldNodeId`); reads `decode.pipelineId`.
- **API:** live frames from `/ws/open/inspector?pipeline=` through `decode/status-feed.ts` `subscribePipelineFeed(…, {frame})` (T-153 contract; never a second socket), `/api/captures`, `/api/captures/{id}/frames`, `POST /api/captures/{id}/parse`, `/api/streams` (served address). Reuses `frame-inspector.ts` pure helpers (`buildTree`, `cycleLeafAt`, `hexBytes`, `asciiChar`, `fitSummaryText`, `crcClass`).
- **Acceptance:**
  - frame → hex+ASCII → tree;
  - clicking a byte selects `byte_index[b][0]` and repeat clicks cycle; clicking a field highlights its `bytes`;
  - gated frames show "withheld" with no bytes;
  - a new frame flashes without stealing the selection;
  - the served address is shown;
  - at 400 px the frames, bytes and tree stack;
  - unit tests cover the ring buffer, the frame view model and byte colouring from layer ranges.

### T-155: Review drawer (Sonnet, medium)
- **Owns:** `ui/src/app/review/*` (`index.ts`, `slice.ts`, `review.css`; new `drawer.ts`, `alarms.ts`, `report.ts`, `history.ts`, `scheduler.ts`, `device.ts`, `bookmarks.ts`).
- **State:** writes `review` (including the Device tab's full control state) and adds a `bookmarks` key, both in `review/slice.ts`; reads `live.view`, `time`, `device` (never writes `device`, which is T-150's).
- **API:** `/api/anomalies*`, `/api/report`, `/api/history` + `/api/floor`, `/api/scheduler*`, `/api/control/*` (via `controls/model.ts` `panelModel`), `/api/bookmarks*`. Reuses the pure helpers of `alarms.ts`, `report.ts`, `scheduler.ts`, `history.ts` and `controls/*`; renders new DOM rather than moving the old classes.
- **Acceptance:**
  - every M0b–M2 panel from the old page is reachable from the drawer: nothing is lost;
  - the report defaults to the current view and timeline range;
  - a selection's History opens the History tab on its region;
  - device controls disable with `not_live` on replay;
  - dismiss and reopen work;
  - unit tests cover the tab model, default-region logic and view models.

### T-156: finish (Sonnet, medium)
Swap `/` to the new app, remove the old DOM code (§7), do the phone-width and theme pass, move fixtures, optionally add the DOM shim, update `ui/README.md`. Acceptance as in `tasks.yaml`, plus: no module in `ui/src` is imported only by deleted code.

**File-ownership matrix** (parallel-safe):

| Path | T-150 | T-151 | T-152 | T-153 | T-154 | T-155 |
|---|---|---|---|---|---|---|
| `app/shell.ts`, `app/shell-slice.ts`, `base.css` shell sections, `app/dock/*` (incl. `api.ts`), `app/capture/*` | own | call `dock/api.ts` | | call `dock/api.ts` | | |
| `app/explore/*` | | own | | | | |
| `app/centre/*`, `waterfall.ts` (additive) | | | own | | | |
| `app/decode/{index,slice,status-feed,pipelines,stages,plots,params,svg}.ts`, `decode/decode.css` | | | | own | call `status-feed.ts` | |
| `app/decode/{inspector,inspector-slice,hexview}.ts`, `decode/inspector.css` | | | | | own | |
| `app/review/*` | | | | | | own |
| `ui/test/app-<panel>.test.ts` | own file | own file | own file | own file | own file | own file |
| `app/{main,state}.ts`, `app/app.css`, `package.json`, `ui/test/run.mjs` | – | – | – | – | – | – |
| `app/{store,net,dom,context,placeholder}.ts` | additive (`net`, `dom`) | – | – | – | – | – |

## Consequences

- No framework to upgrade, and the test harness is unchanged. The price is some hand-written list rendering in each panel, kept manageable by the small `h()` helper and whole-subtree re-renders.
- Two pages coexist until T-156, so old pure helpers are frozen in behaviour for that long.
- The mockup can't be matched fully until the §4.9 gaps land. The UI degrades visibly (interim views, "not served yet") rather than computing signal facts client-side.

## Skeleton measurements

T-149 skeleton, `npm run build` (esbuild 0.28.2, minified, es2020), 2026-09-15:

| Artifact | Size | Budget |
|---|---|---|
| `dist/app.js` | 24.4 KB minified, 9.7 KB gzip (store, shell, net, the reused `waterfall.ts` + `axis.ts` + `controls/{client,freq}.ts`) | ≤ 150 KB / ≤ 45 KB gzip |
| `dist/app.css` | 8.3 KB (esbuild-bundled from `app.css` imports, minified) | ≤ 40 KB |
| old `dist/main.js`, for comparison | 113.9 KB minified | – |

`npm run build`, `npm run typecheck` and `npm test` are green: 13 test files, 157 tests, including `app-store.test.ts` with 13 tests and `app-shell.test.ts` with 9. After the review fixes, the old page's `dist/main.js`, `audio-worklet.js`, `index.html` and `style.css` are byte-identical. The skeleton was not opened against a running `hk serve` (no Rust build in T-149): T-150 checks `/app.html` in a browser as its first step.

## Sources

- `docs/14-ui-rewrite.md` (brief), `ui/mockups/explorer-v3.html` (spec)
- `docs/api.md`, `docs/stream-contract.md` §5, §10, §12.2, §14.3–§14.4
- `crates/hk-api/src/http.rs` (`ROUTES`, `static_file`, CSP), `crates/hk-api/src/query.rs` (`inventory_entry_json`: `refined`, `explanations`), `crates/hk-pipeline/src/family.rs` (`Explanation`, `ExplanationEvidence`), `crates/hk-pipeline/src/recipes/runtime.rs` (pipeline JSON incl. `follow_hops`)
- [ADR-0002](0002-ui-web-vs-native.md), [ADR-0011](0011-decoder-workbench-contracts.md), [ADR-0012](0012-attention-memory-contracts.md)
- Framework sizes are approximate published minified+gzip figures (Preact 10 ≈ 4.5 KB + signals ≈ 1.5 KB; lit 3 ≈ 5.8 KB); unverified against current releases.
