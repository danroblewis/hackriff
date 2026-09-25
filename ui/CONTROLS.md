# Control panel survey (T-051)

Standard control sets of [SDR++](https://github.com/AlexandreRouma/SDRPlusPlus), [SDRangel](https://github.com/f4exb/sdrangel) and [SigDigger](https://github.com/BatchDrake/SigDigger), from their READMEs/manuals and past use (not re-verified in the apps for this note), mapped to the hackriff control API (T-050). Exploration stays first: the panel moves the window and shapes the display; finding and characterising signals stays with detection, inventory and inspect.

| Control (SDR++ / SDRangel / SigDigger) | hackriff | Notes |
|---|---|---|
| Centre frequency (digit-scroll / entry, all three) | `POST /api/control/center` | Entry with units (`101.3M`, `433.92 MHz`, `+25k`); a retune into another content class re-plumbs (≤ 30 s, spinner) |
| Tuning step, shift ◀ ▶ (SDR++ snap interval) | client → `center` | Fixed steps snap to their grid; ½-span/span steps walk a band |
| Scroll-zoom, drag the spectrum (SDR++ FFT, SDRangel spectrum) | client (`surface/input.ts` → `surface/panes.ts`) | Display zoom is client-side, on the unified surface; see **Navigating the surface** below. Panning a viewport onto un-tuned spectrum *offers* an explicit retune (T-444) and never performs one — unless **retune mode** is on (T-1028, below), the one state in which a settled pan/zoom commands the radio |
| Sample rate / span (all; SDRangel adds decimation) | `POST /api/control/rate` | Choices from `device.sample_rates_hz`; decimation stays inside the pipeline |
| Gains LNA/VGA/amp (HackRF source in all three) | `POST /api/control/gains` | Named stages from `device.gain_stages`; generic names for other devices |
| Bias tee (SDR++, SDRangel HackRF source) | `POST /api/control/bias_tee` | Confirm with a DC-on-antenna warning |
| FFT size, averaging, refresh/waterfall rate (all) | `POST /api/control/display` | Limits hard-coded from hk-pipeline (not in the state body) |
| Waterfall min/max, auto-level (SDR++, SigDigger) | client (`surface/surface.ts`) | The display range tracks what the served tiles actually hold (`range_db`), shared by every viewport so two panes cannot shade the same energy differently, and now by the spectrum trace as well. **The manual dB entry is not coming back (T-457):** `Waterfall.setScale(auto, lo, hi)` existed but had *no caller* at the cutover, so T-445 retired an unreachable control rather than a feature in use; and a hand-set range overrides a measurement, which this product declines by default. The honest control is to **state** the range, which the trace readout does |
| Peak / max hold (SDRangel, SigDigger) | backend (the tile fold), drawn client-side (T-457) | Max-hold is what a coarser cell *is* (T-342): a level-n cell is the maximum over the level-0 cells under it, folded server-side. T-457 draws it as the second trace series by reducing the tiles a viewport **already has**, over that viewport's own window — **no accumulator and no new ladder tier**, because the ladder's only reduction already is max-hold and a max of max-holds is a max-hold |
| Bookmarks / frequency manager, markers (SDR++, SDRangel, SigDigger) | `/api/bookmarks` | Add from a click or a selection; jump zooms, or retunes on request |
| Freeze / pause (SDRangel spectrum, SigDigger) | client (the follow-live FAB, per viewport: a press freezes a following viewport and re-pins a frozen one — T-882 retired the toolbar's Live/Paused button) | T-347: holding the view is the client's own time cursor — the same state a scrub leaves — so it is per-viewer and reaches no route. The run-wide `/api/control/pause` is gone: it froze every connected browser's waterfall at once. T-442 made it per **viewport**: a pane's pause *is* its time window, so freezing is a coordinate change and not a mode, and "scrubbed but not paused" is not a state the type can spell |
| Record baseband (SDR++ recorder, SDRangel file sink) | `POST /api/control/record/start`, `stop` | Refused under content-forbidding classes (409 `refused`) |

## Navigating the surface (T-456)

Google-Maps navigation, with the two axes still independently reachable. Every one of these is a view
change: nothing here reaches a device route (T-340's control) **unless retune mode is explicitly on**
(T-1028, stated under the table), and the pyramid levels stay independent per axis (T-434) — a uniform gesture applies one factor to two windows, never one level
to two axes.

| Gesture | Effect |
|---|---|
| Drag | Pans **both** axes. No threshold, and each axis takes its own component of the travel |
| **Shift + drag** | Marks out a **region** (T-458) — a new selection, or the new band for a signal armed by "Adjust band". The view does **not** pan while one is being drawn |
| Wheel | Zooms **both** axes, uniformly, about the cursor |
| Shift + wheel | Zooms **frequency (X)** only |
| Alt / Option + wheel | Zooms **time (Y)** only |
| Ctrl + wheel, Cmd + wheel, trackpad pinch | Uniform zoom, same as a plain wheel |
| Double-click the map | Sends the active pane there |
| Press, right-click, wheel or pinch on a pane | Makes it the **active pane** — outlined on the canvas, and named by the chrome that acts on it (T-1000, docs/23 §10.7) |
| `]` / `[` | Next / previous pane becomes active |
| `1`–`9` | Pane N becomes active |
| `L` | Toggles Live on the active pane (the follow-live FAB's press) |
| `R` (tap) | Latches **retune mode**; tap again to turn it off (the chip in the map controls says which) |
| `R` (held) | Retune mode for one gesture — release restores the default |

**Retune mode (T-1028), the one exception to "no gesture commands the radio".** Off by default, and
off means the rule above exactly: every gesture in this table is view arithmetic and the spy-client
call list stays empty. Turned on — deliberately, and visibly (a lit chip, a banner, a status line on
the pane) — the view's frequency window *is* the tune request: a pan or zoom that **settles** (the
pointer released, a pinch ended, or ~150 ms of stillness for a wheel) issues **one** retune through
the one gated `DeviceAction` path. The latest settled view wins, a request already in flight is never
cancelled, and the next one waits the settle gap. A view **wider than one capture window** tunes the
largest achievable span centred on it (clamped into the tunable range at the band edges) rather than
refusing — the pane keeps showing the wider view, and the coverage fog shows which part of it the
radio took. Frequency only: time, a pane's frozen state, the ring and detection are never touched, so
a frozen pane retunes and stays frozen. `R` was chosen because T-456 already spends Shift, Alt and
Ctrl/Cmd on the zoom/region gestures, and it is the one spare form that can be both held and tapped.

**Why Alt/Option and not Ctrl for the time axis.** Ctrl+scroll is macOS's own zoom gesture
(Accessibility → Zoom, *"Use scroll gesture with modifier keys to zoom"*, whose default modifier is
^Control): when it is enabled the OS consumes the event and the browser is never dispatched a
`wheel` at all, so there is nothing to `preventDefault` and no in-browser test can tell that case
apart from the user not having scrolled. Ctrl+wheel is *also* how Chrome and Safari deliver a
trackpad **pinch**, so binding time to it would make a pinch zoom a single axis. Leaving ctrl
unbound puts a pinch in the uniform branch, where it belongs. Every wheel over the canvas is
`preventDefault`ed regardless, so ctrl/cmd+wheel zooms the surface rather than the page.

**Shift+wheel reads `deltaX`.** A shift-held wheel is delivered as a *horizontal* scroll on macOS,
so the frequency axis takes whichever delta actually carried the scroll — the dominant one, never
the sum (T-407's `clientX + clientY`).

**Why shift+drag for a region, and why it does not collide with shift+wheel.** A wheel and a
captured pointer drag are disjoint event streams: no event can be claimed by both bindings, and you
cannot be mid-gesture in both. The two readings are one idea rather than two — shift confines the
gesture to a *region of frequency* instead of sliding the whole view. The three alternatives were
rejected for reasons the page cannot see at runtime, which is the class of failure T-456 rejected
ctrl+wheel for:

- **Ctrl+drag** is macOS's secondary click. The browser sends `contextmenu` and `button === 2`, so
  the stroke silently becomes "open the menu" — the same invisible failure as ctrl+scroll, in its
  pointer form.
- **Alt/Option+drag** is Chrome's copy-drag modifier, is grabbed by common Linux window managers to
  move the window, and already means *time axis* on the wheel — the one real collision available.
- **Right-drag** would have to fight the context menu, this surface's only route to
  Promote / Delete / Adjust band / Reset band.
- **A mode toggle** is a state you can be in without noticing; the surface already has one
  (Live/Paused). A mode is still right for naming *which* signal a band override applies to, where
  the target has to be stated anyway — that is what "Adjust band" arms.

The modifier is latched at the press and never re-read, so letting go of shift mid-stroke cannot
turn the region into a pan of the view it is being drawn on. A **tap is never a region**: the gate is
the net press-to-release displacement as a `Math.hypot` distance (never `dx + dy`, T-407's own
defect, which cancels to zero on an ordinary down-left stroke) plus a non-degenerate extent on both
axes. All of this is confirmed against a real Chrome in `ui/e2e/surface-region.e2e.mjs`, which reads
the `shiftKey` flag *on the event the canvas received* rather than inferring it from what the view
did.

## What T-445's cutover removed, and where each thing went

The two bespoke edge scrubbers (the left time navigator and the bottom frequency navigator) and the
separate region-over-time history view are retired into the one surface (docs/16 §8.5). The controls
they carried did not disappear with them:

| Retired control | Where it is now |
|---|---|
| Time navigator: scrub, zoom the time span, LIVE/PAUSED | The surface's own time axis (drag, Alt+wheel) and the per-viewport follow-live **FAB** (T-882) |
| Time navigator: compressed history of the selected band | The surface itself — history *is* the surface, at whatever level the viewport resolves to |
| Frequency navigator: set centre and span across the device range | Pan and zoom the viewport, or the **map strip** along the canvas's bottom (double-click sends the active viewport there) |
| Frequency navigator: lit segment per active capture window | The map's per-SDR live segments, read through the same `activeWindows` (T-443) |
| Frequency navigator: region-select → retune | The **Retune** offer beside the viewport (T-444). It is an offer and a separate press, and it refuses (`"moved"`) if the viewport moved after the label was drawn |
| Review drawer → "Spectrum grid" (region over time) | The surface. That tab drew `GET /api/history` with a second, hand-written colormap — T-397's divergence, in the repo twice |
| The live FFT plot above the waterfall | The **trace strip** above each viewport (T-457, `surface/trace.ts`): the spectrum at that viewport's own time position, plus the max-hold over its window |
| The live waterfall's frequency axis strip | **No home yet** — see below |

**Three things had no home on the canvas. Two are now settled; one still needs a decision rather
than a quiet deletion:**

1. ~~**The instantaneous spectrum trace**, the client-side **max-hold** and the **manual dB range
   entry**~~ — **settled by T-457.** The trace is back as a strip above each viewport (see *The
   spectrum trace* below); max-hold is drawn from the tile fold; the manual dB range is deliberately
   not restored.
2. ~~**Drag-to-select a region** and the **Confirmed band's draggable edges**~~ — **settled by
   T-458.** The gesture is **shift + drag** (see *Navigating the surface* above). A stroke is a new
   `POST /api/selections` region, or — after "Adjust band" on a Confirmed row — that row's new
   `PUT /api/inventory/{id}/band` override. The **edge handles** T-193 drew are not coming back:
   the override is now set by marking out the band you want rather than by dragging two handles,
   which is one gesture instead of two hit-targets and works at 400 px. "Reset band" still clears it.
3. **Frequency and time axis ticks with labels.** The old `.axis` strip is gone; the surface states
   each viewport's window and level in its chrome line, which is a readout rather than a ruler.

### The spectrum trace, restored and time-addressable (T-457)

A strip is carved off the **top of each viewport's rectangle** — never painted over it, because an
overlay covering the newest rows would make "the top of the pane is the newest row" false. In it:

- **slice** — the spectrum across the whole viewport **at that viewport's own time position**
  (`box.t1Ns`). Following the edge, that is the newest delivered row, straight off the spectrum
  stream; scrubbed into the past, it is the pyramid's row of cells at that instant. The readout
  names which, and the cell's duration when it is a cell, so a max over a second is never passed off
  as an instant. A trace pinned to *now* is the naive version and is explicitly not what this is.
- **max-hold** — the column-wise maximum over the viewport's **whole** window.

Both are drawn only where data exists: a column nothing answered for emits nothing, because
*unobserved is not quiet* and a line across a gap claims a measurement nobody took. Both share the
surface's two axes — x through the renderer's own `toClip`, y through the one measured display range
the ramp uses — so a peak on the trace sits above the column it paints. The **Trace** button hides
the strip and gives the space back to the viewport.

**The strip is a READOUT, not a control: it passes every pointer event through to its pane.** A
press, drag, wheel, hover, click or right-click that lands in the strip is treated as one at the
**top edge of the pane below it** — the same frequency, at the pane's newest instant, which is
exactly the instant the strip is a spectrum of. So drag still pans, shift+drag still marks out a
region, plain/shift/alt wheel still zoom as *Navigating the surface* says: nothing about a gesture
changes because it started a few pixels higher.

The alternative — the strip handling pointers with a meaning of its own — was rejected twice over.
The obvious meaning for a vertical drag on a dB axis is *set the display range by hand*, which is
the control this table says is deliberately not restored; and a second gesture vocabulary on one
canvas is T-412's wheel-zoom mismatch waiting to happen. Saying this explicitly is the point: when
the strip was neither — when it simply resolved to no pane — it silently swallowed **every** drag
that began in it, which is how T-457 and T-458 broke on merge while each was green alone.

**Skipped, and why:**
- **Transmit** (SDRangel TX device sets, replay-to-TX): receive only; the API has no TX route (C37 gated).
- **Manual demodulator mode, bandwidth, squelch, de-emphasis** (SDR++ radio module, SDRangel channel plugins): parameters are estimated from the signal (T-012; Listen is T-043), never picked by hand.
- **Symbol-rate/clock inspector knobs** (SigDigger inspector): blind estimation (hk-estimate) and inspect cover them.
- **PPM / clock correction, DC and IQ correction** (SDRangel, SDR++): calibration state (T-021) and the pipeline, not panel sliders.
- **FFT window choice, colour map, reference-level/range dials**: fixed for now; the window is not exposed by the API.
- **Baseband filter, antenna port, device picker**: not in the control API (the source picks the filter from the rate; one device per server).
