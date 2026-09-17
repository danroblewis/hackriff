# Control panel survey (T-051)

Standard control sets of [SDR++](https://github.com/AlexandreRouma/SDRPlusPlus), [SDRangel](https://github.com/f4exb/sdrangel) and [SigDigger](https://github.com/BatchDrake/SigDigger), from their READMEs/manuals and past use (not re-verified in the apps for this note), mapped to the hackriff control API (T-050). Exploration stays first: the panel moves the window and shapes the display; finding and characterising signals stays with detection, inventory and inspect.

| Control (SDR++ / SDRangel / SigDigger) | hackriff | Notes |
|---|---|---|
| Centre frequency (digit-scroll / entry, all three) | `POST /api/control/center` | Entry with units (`101.3M`, `433.92 MHz`, `+25k`); a retune into another content class re-plumbs (≤ 30 s, spinner) |
| Tuning step, shift ◀ ▶ (SDR++ snap interval) | client → `center` | Fixed steps snap to their grid; ½-span/span steps walk a band |
| Scroll-zoom, drag the spectrum (SDR++ FFT, SDRangel spectrum) | client (`axis.ts`) | Display zoom is client-side; panning past the band offers an explicit retune |
| Sample rate / span (all; SDRangel adds decimation) | `POST /api/control/rate` | Choices from `device.sample_rates_hz`; decimation stays inside the pipeline |
| Gains LNA/VGA/amp (HackRF source in all three) | `POST /api/control/gains` | Named stages from `device.gain_stages`; generic names for other devices |
| Bias tee (SDR++, SDRangel HackRF source) | `POST /api/control/bias_tee` | Confirm with a DC-on-antenna warning |
| FFT size, averaging, refresh/waterfall rate (all) | `POST /api/control/display` | Limits hard-coded from hk-pipeline (not in the state body) |
| Waterfall min/max, auto-level (SDR++, SigDigger) | client (`waterfall.ts`) | Auto (floor/peak tracking) or manual dB |
| Peak / max hold (SDRangel, SigDigger) | client | Max-hold trace over the spectrum; resets on retune |
| Bookmarks / frequency manager, markers (SDR++, SDRangel, SigDigger) | `/api/bookmarks` | Add from a click or a selection; jump zooms, or retunes on request |
| Freeze / pause (SDRangel spectrum, SigDigger) | client (the time navigator's LIVE/PAUSED control) | T-347: holding the view is the client's own time cursor — the same state a scrub leaves — so it is per-viewer and reaches no route. The run-wide `/api/control/pause` is gone: it froze every connected browser's waterfall at once |
| Record baseband (SDR++ recorder, SDRangel file sink) | `POST /api/control/record/start`, `stop` | Refused under content-forbidding classes (409 `refused`) |

**Skipped, and why:**
- **Transmit** (SDRangel TX device sets, replay-to-TX): receive only; the API has no TX route (C37 gated).
- **Manual demodulator mode, bandwidth, squelch, de-emphasis** (SDR++ radio module, SDRangel channel plugins): parameters are estimated from the signal (T-012; Listen is T-043), never picked by hand.
- **Symbol-rate/clock inspector knobs** (SigDigger inspector): blind estimation (hk-estimate) and inspect cover them.
- **PPM / clock correction, DC and IQ correction** (SDRangel, SDR++): calibration state (T-021) and the pipeline, not panel sliders.
- **FFT window choice, colour map, reference-level/range dials**: fixed for now; the window is not exposed by the API.
- **Baseband filter, antenna port, device picker**: not in the control API (the source picks the filter from the rate; one device per server).
