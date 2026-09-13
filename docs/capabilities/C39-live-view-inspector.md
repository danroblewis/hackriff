# C39 · live-view-inspector
> Layer G — Specialised · Status: draft (taxonomy draft 2026-09-13) · Depends on: C02, C07, C09, C10, C13, C14, C20, C21, C25, C26, C27, C28 ("reads everything", docs/06 §2.1) · Used by: user; sends commands to C04, C25, C28

## Purpose
The interactive surface:
- live spectrum, waterfall and DPX-style persistence;
- region-over-time history browsing;
- the signal table;
- a per-signal **inspector** (constellation, IF histogram, symbols, bits, cyclic spectrum) bound to an emitter's records.

It replaces the tuning-first VFO model with an object model: signals you list, tag and revisit (`docs/03 §5.1`). It serves workflow steps 1, 3 and 5–6. Web versus native is a Phase 3 ADR; this card lists considerations only.

## Interface
- **Inputs (provisional objects):**
  - SpectrumFrames from C07: ~4.9k FFT/s at 20 Msps (docs/06 C07), so the display must decimate. Persistence and SK arrive with them.
  - SweepFrames from C02: 0–6 GHz about once per second (`docs/01 §7.3`).
  - Detections/Tracks.
  - C27 emitter queries, C26 history tiles, C25 snippets.
  - Inspector streams: soft symbols, bits and EVM/lock (C20/C21), parameters (C13/C14).
- **Outputs (same API remote clients use):** dwell/tune, record, open inspector (spawn DDC/demod), label, mute emitter, scan-plan edits.
- **Rates (estimates):**
  - Render 30–60 fps.
  - Waterfall 10–30 rows/s. UI stats cadence ~10 Hz (`docs/01 §7.1`); spectrogram storage assumes 30 lines/s (`docs/02 §3.1`).
  - Remote: 4096 bins × 1 byte × 30 rows/s ≈ 123 KB/s (arithmetic).
- **Latency:** input-to-visual <100 ms (estimate). The UI never back-pressures DSP.

## Methods
- **Waterfall:** GPU texture ring plus colour-map shader. Decimate bins → pixels by **max** (optionally mean too) so bursts and carriers survive. Auto-range from the C08 floor with hysteresis.
- **Persistence:** H[f, P_dB] with decay H ← βH + hits, computed beside the FFT on the GPU and sent as an image (`docs/04 §3.5`). Show a stated POI duration (`docs/02 §6`).
- **Dual resolution** (e.g. 1 kHz and 25 Hz bins) for bursts versus carriers (`docs/04 §3.5`).
- **Overlays:** C09 boxes and SigMF annotations (IQEngine pattern), with provenance badges (clip, spur-mask, IMD suspect, "unexpected here").
- **Signal table:** sortable, filterable, linked to recordings (Aaronia/CRFS, `docs/03 §5.2`).
- **Inspector:** SigDigger model plus the `docs/04 §11.2` drill-down list. Automation proposes parameters; the user only nudges.
- **History:** region × time → C26 tiles at zoom-appropriate resolution; same widgets as live.
- **UI ADR considerations (undecided):**
  - **Web:**
    - Maia SDR (Rust REST + WASM/WebGL2, phone as display), OpenWebRX, IQEngine, FutureSDR/RustRadio WASM (`docs/03 §2.4`, `§1.5`).
    - Free remote clients and a GPL-isolating process boundary.
    - Local display needs a kiosk browser on Jetson; frame time unmeasured.
  - **Native ImGui:** SDR++ has "best-in-class responsiveness" (`docs/03 §2.1`). Lowest local latency, but remote needs a second path.
  - **Hybrid:** OpenDigitizer compiles one Dear ImGui codebase native and WASM on GR4 (`docs/03 §1.2`).
  - **Either way:** API-first control plane (`docs/01 §7.1`, item 4). Beware GL-stack lag seen on Pi 5 SDR GUIs (`docs/02 §3.3`).

## Platform constraints
- **Display:** 5–7" touch at 2–4 W, a real share of a ~15–38 W tier-B budget (`docs/02 §7.2`). Offer dim/off and headless modes.
- **GPU memory:** 8 GB unified, shared with cuFFT/PFB and ML (`docs/02 §3.3`).
- **Touch, no hover:** PortaPack's 240×320 dense config screens are the anti-pattern (`docs/01 §7.2`).
- **Usable band:** HackRF One gives ~15–18 MHz of 20 MHz after skirts, plus a DC spike (`docs/01 §7.3`). Mask or label them.

## Prior art and reuse
- **SigDigger/SuWidgets:** inspector pattern, OpenGL waterfall. Single maintainer; licence: check (`docs/03 §3.4`).
- **Maia SDR:** WASM/WebGL2 waterfall, REST, phone UI. MVP; licence: check (`docs/03 §2.4`).
- **SDR++:** ImGui responsiveness, module API. Active; licence: check.
- **IQEngine:** web spectrogram, annotation editing. Maintained; licence: check.
- **inspectrum:** minimal offline cursors (`docs/03 §3.4`).
- **gr-fosphor and Tektronix DPX:** persistence references (`docs/03 §1.1`, `§3.9`).
- **OpenDigitizer:** v1.0.0 2026-06 (`docs/03 §1.2`).
- **`tools/sweep_plot.py`:** the user's matplotlib waterfall from `hackrf_sweep` CSV; an offline reference renderer. Carry its load rules (drop the first settling sweep and partial sweeps; rows are unsorted) into the C02/C39 contract.

## Pitfalls
- **UI coupled to radio events** causes lockups (Mayhem, `docs/01 §5`). Decouple DSP and render with drop-oldest queues.
- **Settings lost switching views** (Looking Glass, `docs/01 §3.4`).
- **Wide sweeps "appear frozen":** show progress and row age.
- **Mean-decimation hides bursts; auto-scale flicker.**
- **Spurs, DC spike and IMD ghosts shown as real:** add badges.
- **A slow remote client stalls the pipeline:** per-client drop policy.
- **Tuning-first creep:** the primary action is "select signal", not "set frequency and mode".

## Testing
- **Replay harness:** SigMF → C01 file source → pipeline → offscreen render. Golden-image diff with perceptual tolerance.
- **Frame time:** p99 ≤33 ms at 30 fps (≤16.7 ms at 60) during 20 Msps replay with ML loaded, in each power mode.
- **Non-blocking:** throttle or kill a client; assert zero C03 drops and bounded queues.
- **Burst visibility:** synthetic 1 ms low-duty bursts must show in persistence and overlays. Assert a mean-only waterfall misses them.
- **Scripted input replay:** a recorded touch/command sequence yields the expected commands and screenshots. Precedent: Mayhem's `screenshot`/`touch`/`accessibility_readall` shell (`docs/01 §3.6`).
- **Inspector:** a known QPSK/FSK fixture gives the expected constellation cluster count and symbol-rate readout.
- **Live hardware:** outdoor readability, touch, display battery draw.

## Example use cases
Provisional until docs/06 §3 mapping. C39 is substrate and rarely listed:
- AWARE-031 — Long-term noise-floor trend logger
- RESEARCH-076 — Browser-based IQ exploration
- RESEARCH-005 — Blind signal detection with gr-inspector
- RESEARCH-007 — Catalog unknowns against Sig ID Wiki
- RESEARCH-050 — SDR as spectrum analyzer / power survey
- SPACE-015 — "Space weather now" local dashboard
- PROP-052 — Real-time passive radar with KrakenSDR/blah2

## Open questions
- **Web versus native versus hybrid:** a spike should measure Chromium/WebGL2 against ImGui frame time and latency on Orin Nano with the same 20 Msps replay.
- **Remote viewing:** is it a first-release requirement? It shifts the ADR weighting.
- **Where persistence and decimation run:** server-side GPU (fixed client bandwidth) or client-side?
- **Scope:** docs/06 C39 bundles live view, history browser, signal table and inspector. Split the inspector out? "Reads everything" hides dependency order.
- **Dashboards and maps:** no docs/06 capability covers them (SPACE-015, C31 heat maps, C30 explanations). C39, or a new one?

## Reading list
1. `docs/03 §5.2 "Best UX ideas that already exist (steal these)"`.
2. `docs/03 §5.1 "Concrete problems"`.
3. `docs/03 §3.4 "Protocol reverse engineering and signal inspection"` — SigDigger, IQEngine, inspectrum.
4. `docs/03 §2.4 "Web-based and embedded receivers"` — Maia SDR.
5. `docs/04 §3.5 "Time–frequency analysis"` — persistence, dual resolution.
6. `docs/04 §11.2 "Mapping to an exploration device"` — inspector drill-down.
