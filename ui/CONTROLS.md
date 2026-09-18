# Control panel survey (T-051)

Standard control sets of [SDR++](https://github.com/AlexandreRouma/SDRPlusPlus), [SDRangel](https://github.com/f4exb/sdrangel) and [SigDigger](https://github.com/BatchDrake/SigDigger), from their READMEs/manuals and past use (not re-verified in the apps for this note), mapped to the hackriff control API (T-050). Exploration stays first: the panel moves the window and shapes the display; finding and characterising signals stays with detection, inventory and inspect.

| Control (SDR++ / SDRangel / SigDigger) | hackriff | Notes |
|---|---|---|
| Centre frequency (digit-scroll / entry, all three) | `POST /api/control/center` | Entry with units (`101.3M`, `433.92 MHz`, `+25k`); a retune into another content class re-plumbs (≤ 30 s, spinner) |
| Tuning step, shift ◀ ▶ (SDR++ snap interval) | client → `center` | Fixed steps snap to their grid; ½-span/span steps walk a band |
| Scroll-zoom, drag the spectrum (SDR++ FFT, SDRangel spectrum) | client (`surface/input.ts` → `surface/panes.ts`) | Display zoom is client-side, on the unified surface; see **Navigating the surface** below. Panning a viewport onto un-tuned spectrum *offers* an explicit retune (T-444) and never performs one |
| Sample rate / span (all; SDRangel adds decimation) | `POST /api/control/rate` | Choices from `device.sample_rates_hz`; decimation stays inside the pipeline |
| Gains LNA/VGA/amp (HackRF source in all three) | `POST /api/control/gains` | Named stages from `device.gain_stages`; generic names for other devices |
| Bias tee (SDR++, SDRangel HackRF source) | `POST /api/control/bias_tee` | Confirm with a DC-on-antenna warning |
| FFT size, averaging, refresh/waterfall rate (all) | `POST /api/control/display` | Limits hard-coded from hk-pipeline (not in the state body) |
| Waterfall min/max, auto-level (SDR++, SigDigger) | client (`surface/surface.ts`) | The display range tracks what the served tiles actually hold (`range_db`), shared by every viewport so two panes cannot shade the same energy differently. **T-445 dropped the manual dB entry** with the retired waterfall — see the findings note below |
| Peak / max hold (SDRangel, SigDigger) | backend (the tile fold) | Max-hold is what a coarser cell *is* (T-342): a level-n cell is the maximum over the level-0 cells under it, folded server-side. **T-445 dropped the client-side max-hold trace** with the spectrum plot — see the findings note below |
| Bookmarks / frequency manager, markers (SDR++, SDRangel, SigDigger) | `/api/bookmarks` | Add from a click or a selection; jump zooms, or retunes on request |
| Freeze / pause (SDRangel spectrum, SigDigger) | client (the surface's Live/Paused button, per viewport) | T-347: holding the view is the client's own time cursor — the same state a scrub leaves — so it is per-viewer and reaches no route. The run-wide `/api/control/pause` is gone: it froze every connected browser's waterfall at once. T-442 made it per **viewport**: a pane's pause *is* its time window, so freezing is a coordinate change and not a mode, and "scrubbed but not paused" is not a state the type can spell |
| Record baseband (SDR++ recorder, SDRangel file sink) | `POST /api/control/record/start`, `stop` | Refused under content-forbidding classes (409 `refused`) |

## Navigating the surface (T-456)

Google-Maps navigation, with the two axes still independently reachable. Every one of these is a view
change: nothing here reaches a device route (T-340's control), and the pyramid levels stay
independent per axis (T-434) — a uniform gesture applies one factor to two windows, never one level
to two axes.

| Gesture | Effect |
|---|---|
| Drag | Pans **both** axes. No threshold, and each axis takes its own component of the travel |
| Wheel | Zooms **both** axes, uniformly, about the cursor |
| Shift + wheel | Zooms **frequency (X)** only |
| Alt / Option + wheel | Zooms **time (Y)** only |
| Ctrl + wheel, Cmd + wheel, trackpad pinch | Uniform zoom, same as a plain wheel |
| Double-click the map | Sends the active pane there |

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

## What T-445's cutover removed, and where each thing went

The two bespoke edge scrubbers (the left time navigator and the bottom frequency navigator) and the
separate region-over-time history view are retired into the one surface (docs/16 §8.5). The controls
they carried did not disappear with them:

| Retired control | Where it is now |
|---|---|
| Time navigator: scrub, zoom the time span, LIVE/PAUSED | The surface's own time axis (drag, Alt+wheel) and the per-viewport **Live/Paused** button |
| Time navigator: compressed history of the selected band | The surface itself — history *is* the surface, at whatever level the viewport resolves to |
| Frequency navigator: set centre and span across the device range | Pan and zoom the viewport, or the **map strip** along the canvas's bottom (double-click sends the active viewport there) |
| Frequency navigator: lit segment per active capture window | The map's per-SDR live segments, read through the same `activeWindows` (T-443) |
| Frequency navigator: region-select → retune | The **Retune** offer beside the viewport (T-444). It is an offer and a separate press, and it refuses (`"moved"`) if the viewport moved after the label was drawn |
| Review drawer → "Spectrum grid" (region over time) | The surface. That tab drew `GET /api/history` with a second, hand-written colormap — T-397's divergence, in the repo twice |
| The live waterfall's frequency axis strip | **No home yet** — see below |

**Three things have no home on the canvas, and they need a decision rather than a quiet deletion:**

1. **The instantaneous spectrum trace** (the live FFT plot above the old waterfall), and with it the
   client-side **max-hold** and the **manual dB range entry**. The surface draws folded cells over
   time; a live trace of the current frame is a different picture, not a zoom level of this one.
2. **Drag-to-select a region** (`POST /api/selections` from the waterfall) and the **Confirmed
   band's draggable edges** (T-193's user-band override). On this surface a drag pans (T-456), so a
   selection gesture needs a modifier or a mode that is not yet designed. Selections can still be
   made from the **capture band's** time drag, and are drawn on the surface as stroked boxes.
3. **Frequency and time axis ticks with labels.** The old `.axis` strip is gone; the surface states
   each viewport's window and level in its chrome line, which is a readout rather than a ruler.

**Skipped, and why:**
- **Transmit** (SDRangel TX device sets, replay-to-TX): receive only; the API has no TX route (C37 gated).
- **Manual demodulator mode, bandwidth, squelch, de-emphasis** (SDR++ radio module, SDRangel channel plugins): parameters are estimated from the signal (T-012; Listen is T-043), never picked by hand.
- **Symbol-rate/clock inspector knobs** (SigDigger inspector): blind estimation (hk-estimate) and inspect cover them.
- **PPM / clock correction, DC and IQ correction** (SDRangel, SDR++): calibration state (T-021) and the pipeline, not panel sliders.
- **FFT window choice, colour map, reference-level/range dials**: fixed for now; the window is not exposed by the API.
- **Baseband filter, antenna port, device picker**: not in the control API (the source picks the filter from the rate; one device per server).
