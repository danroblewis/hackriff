# 09 — Risks and spikes

*Architecture planning, Phase 4. Drafted 2026-09-13. Status: **PROVISIONAL** — this ranks the assumptions that could sink the design and defines a spike for each. The brief checkpoints here for the user to pick which spikes to run; autonomously, §3 gives a recommended order. Spikes produce throwaway code in `spikes/`, not product code.*

## 1. Risk ranking

Ranked by (impact if the assumption is false) × (uncertainty). "Blocks" names the ADR/capability that stays provisional until the spike resolves it.

| # | Risk | If false… | Uncertainty | Blocks | Spike |
|---|---|---|---|---|---|
| R1 | **Live pipeline reconfiguration** in the chosen runtime — adding/removing a demod chain on a running capture without a rebuild or a capture gap | The core's central premise fails; fall back to owned dataflow or reconsider GR4 | Medium (FutureSDR unproven for us; owned fallback exists) | ADR-0001 | **S1** |
| R2 | **Sustained 20 Msps USB ingest + GPU FFT/channelizer within the Jetson power budget**, no dropped samples | Must decimate earlier, drop the GPU channelizer, or accept a narrower real-time span | Medium (USB 2.0 on ARM has known quirks, [docs/02 §3.3](02-sdr-landscape.md)) | ADR-0001, ADR-0007 | **S2** |
| R3 | **Detection is trustworthy in urban overload** — CFAR + spur mask + provenance keep the false-alarm rate low on an 8-bit no-preselector front end | The inventory fills with intermod/image ghosts; the attack map explains phantoms | **High** (this is the #1 real-world HackRF complaint, [docs/01 §5](01-hackrf-and-portapack.md)) | ADR-0005, C05, C09 | **S4** |
| R4 | **Blind symbol-rate/modulation estimation works on real 8-bit HackRF captures**, not just clean synthetic IQ | Unknown-signal exploration (the differentiator) underdelivers; more manual work | **High** (docs warn 8-bit + OTA is hard, [docs/04 §4–5](04-radio-engineering-and-signals-analysis.md)) | C14, C15 | **S5** |
| R5 | **Web/WebGL2 waterfall holds an acceptable frame rate** at 20 Msps persistence, on-device and remote | Add a native shell for the on-device waterfall (the ADR-0002 fallback) | Medium (Maia SDR is evidence it works; our persistence load is heavier) | ADR-0002 | **S3** |
| R6 | **Battery life and thermals** are acceptable in a sealed cyberdeck enclosure at dwell/decode load | Enclosure/cooling/battery redesign; MAXN unusable sealed | Medium–High (Jetson at 25 W in a sealed case is hot, [docs/02 §7.2](02-sdr-landscape.md)) | ADR-0009 | **S6** |
| R7 | **Two HackRFs on a shared 10 MHz clock are phase-coherent enough** for passive radar / basic DF | C35 and coherent-DF use cases stay `needs-other-sdr` (no change to the base device) | Medium (unverified, docs/06 §5) | C32, C35 fit | **S7** |
| R8 | Trunking control-channel span fits the 20 MHz window often enough to be useful | Some 700/800 MHz systems need the scheduler to time-share or a second radio | Low–Medium ([docs/01 §7.3](01-hackrf-and-portapack.md)) | C23 | folded into S4/roadmap, no standalone spike |

## 2. Spikes

Each: **hypothesis**, **setup**, **pass/fail**, **effort**, **where it runs**.

### S1 — Live pipeline reconfiguration
- **Hypothesis:** in the chosen runtime (FutureSDR first), a demod chain can be added to and removed from a graph that is actively consuming samples, with no capture gap and no rebuild.
- **Setup:** a minimal Rust graph: file/HackRF source → ring buffer → FFT sink running continuously; at runtime, attach a DDC+FM-demod node on a command, run it, detach it, attach a different one. Repeat under load. Try the same in an owned mini-dataflow.
- **Pass/fail:** PASS if attach/detach happens within one buffer period with zero dropped input samples and no restart, for ≥100 cycles. FAIL otherwise.
- **Effort:** ~2–3 days (FutureSDR learning curve dominates).
- **Runs on:** Mac now (file source); confirm on Jetson later.

### S2 — Sustained throughput + GPU FFT within power
- **Hypothesis:** the Jetson sustains 20 Msps HackRF ingest plus a continuous GPU FFT (and a first PFB channelizer) with no dropped samples inside a ≤15 W mode.
- **Setup:** Orin Nano Super + HackRF One; Rust ingest → unified-memory buffer → cuFFT 4096-pt at full rate → drop; measure dropped-sample counters, GPU/CPU load, and board power (`tegrastats`) across 7/15/25 W modes; add a PFB and re-measure.
- **Pass/fail:** PASS if zero sustained sample drops for ≥1 h at ≤15 W with FFT, and channelizer runs at ≤25 W. FAIL if drops or power exceeds envelope.
- **Effort:** ~3–5 days (needs the Jetson).
- **Runs on:** **Jetson required.**

### S3 — Waterfall frame rate (web)
- **Hypothesis:** a WASM/WebGL2 client renders the persistence waterfall at ≥30 fps from the core's spectrum stream, both in the on-device kiosk browser and a remote phone.
- **Setup:** core streams SpectrumFrames over the ADR-0004 contract; a minimal WebGL2 texture-scroll waterfall consumes them; measure fps and latency on the Jetson's browser and a phone over Wi-Fi.
- **Pass/fail:** PASS at ≥30 fps on-device and ≥15 fps remote with full persistence. FAIL → invoke the ADR-0002 native-shell fallback for the on-device waterfall.
- **Effort:** ~2–3 days.
- **Runs on:** Mac for the render prototype; confirm on Jetson.

### S4 — Detection quality in urban overload
- **Hypothesis:** noise-floor (FCME/min-stats) + OS-CFAR + spur mask + the retune/gain-step tests keep the false-alarm rate at or below target in a real city capture, distinguishing real emitters from intermod/image ghosts.
- **Setup:** capture wideband IQ at a busy urban location (FM/pager/cellular present) at several gains; run the detection chain; label ghosts by the gain-step (IM3 moves ~3×) and retune tests; compute false-alarm rate and missed-detection on known emitters. Compare with an FM notch filter in line.
- **Pass/fail:** PASS if ghosts are flagged/suppressed and false-alarm rate ≤ a set threshold (e.g. <1 false emitter/MHz/hour after masking) while known emitters are still detected. FAIL → prioritise the preselector/notch accessory earlier.
- **Effort:** ~4–6 days (capture + algorithm tuning); the highest-value spike.
- **Runs on:** Mac + HackRF now (capture + offline processing); a notch filter helps.

### S5 — Blind estimation on real 8-bit captures
- **Hypothesis:** symbol-rate (envelope/delay-multiply/cyclic), FSK deviation, and coarse modulation family are recovered within tolerance on real HackRF captures of known signals (e.g. a 433 MHz sensor, an AIS burst, a pager), not just synthetic IQ.
- **Setup:** capture known-truth signals; run C13/C14 estimators; compare estimated symbol rate/deviation/family to ground truth across SNRs; include a genuinely unknown burst and check the open-set path returns "unknown" rather than a false label.
- **Pass/fail:** PASS if symbol rate within ±1%, deviation within tolerance, family correct above a stated SNR, and unknowns flagged. FAIL → lean harder on decoder trial-decoding and priors; treat blind estimation as assistive not authoritative.
- **Effort:** ~4–6 days.
- **Runs on:** Mac + HackRF now.

### S6 — Battery and thermal in enclosure
- **Hypothesis:** at dwell/decode load the Jetson+HackRF stay within thermal limits in the target enclosure and hit a useful runtime on a chosen battery.
- **Setup:** run the S2 workload in a mock enclosure with the intended cooling; log temperature and throttling and power; extrapolate runtime for candidate battery capacities; measure the low-power survey mode too.
- **Pass/fail:** PASS if no thermal throttling at 25 W for ≥30 min in the enclosure and low-power mode gives the target runtime. FAIL → bigger heatsink/fan, lower default mode, or accept shorter dwell bursts.
- **Effort:** ~3–4 days; needs the Jetson + enclosure mock-up.
- **Runs on:** **Jetson required**, later (needs enclosure).

### S7 — Two-HackRF coherence (optional)
- **Hypothesis:** two HackRFs sharing a 10 MHz reference stay phase-coherent enough after a retune-calibration for basic passive radar / 2-channel DF.
- **Setup:** two HackRFs, common CLKIN; a calibration tone; measure residual phase drift over time and across retunes.
- **Pass/fail:** PASS if residual phase is stable enough for a cross-correlation range-Doppler map on a strong FM illuminator. FAIL → C32/C35 stay `needs-other-sdr` (no base-device change).
- **Effort:** ~3–4 days; two HackRFs.
- **Runs on:** Mac + 2× HackRF; low priority.

### S8: Cost of a wrapped GNU Radio decoder (T-556, done 2026-09-22)
- **Hypothesis (docs/18 §2.4):** a GNU Radio OOT wrapped as a subprocess plugin (ADR-0003/0010)
  is a cheap way to add decoder coverage, and a shared GR host could amortise its runtime.
- **Setup:** gr-lora_sdr (SIGNAL-053) and gr-satellites (SIGNAL-034) were wrapped behind the §9
  plugin contract: `hackriff-v1` cf32 in, NDJSON out, run under the real `PluginInstance`.
  - The LoRa fixture is synthetic with a hidden truth list; gr-satellites is checked for parity
    with its stock CLI.
  - Measured: RSS/CPU/start-up, kill and restart, drops, and one process versus two.
  - The Jetson side was assessed from the Ubuntu 22.04 arm64 package indices, not run.
- **Result: runtime PASS, integration mixed.**
  - Runtime: 27/55 MB private memory, 2.4–3 %/11.5 % of an M3 core at real time, ready in
    0.3/1.6 s warm and 4–12 s after a relink.
  - Exact host-time stamping needed an 8-line fork patch to gr-lora_sdr and is **infeasible for
    gr-satellites** (arrival-stamped only).
  - Runtime install on JetPack 6: ~1.1 GB, 375 packages, with a GR/pybind11 version skew
    against the Mac.
  - A shared GR host saves ~one runtime floor (~38 MiB) per decoder and costs fault isolation:
    **per-decoder processes stand.**
  - Per-decoder cost after the first: 2–4 h to a parity-checked arrival-stamped wrap; 1.5–3 days
    to product grade where the OOT can carry sample offsets.
- **Unblocks:** docs/18 §2.4 (now measured); the direction that GNU Radio's reference set is a
  specification source for native recipes, with wrapping a narrow opt-in tier. Write-up:
  [`spikes/t556-gnuradio-wrap/README.md`](../spikes/t556-gnuradio-wrap/README.md).

## 3. Recommended spike order (provisional — user picks)

1. **S4 (detection in overload)** and **S5 (blind estimation)** first — they run on the Mac + HackRF the user has *now*, need no Jetson, and de-risk the two highest-uncertainty, highest-impact assumptions (R3, R4). They also produce the first real SigMF fixtures for the test suite ([docs/10](10-test-strategy.md)).
2. **S1 (live reconfiguration)** next — also Mac-runnable (file source), unblocks the core runtime choice (ADR-0001) before any Jetson work.
3. **S3 (web waterfall)** — Mac-runnable render prototype, unblocks the UI direction (ADR-0002).
4. **S2 (throughput+power)** — once the Jetson arrives; unblocks ADR-0001/0007 on real hardware.
5. **S6 (battery/thermal)** — after S2 and an enclosure mock-up.
6. **S7 (coherence)** — optional, only if passive-radar/DF is wanted on the base device.

**What each unblocks:** S1→ADR-0001 ACCEPTED; S2→ADR-0001/0007 ACCEPTED; S3→ADR-0002 ACCEPTED (or native-shell fallback); S4→ADR-0005 + C05/C09 thresholds; S5→C14/C15 confidence; S6→ADR-0009 battery/enclosure; S7→C32/C35 fit value.

## 4. Non-spike risks to watch

- **GR4 governance / FutureSDR maintainer bandwidth** — ecosystem risk, not a spike; mitigated by the owned-dataflow fallback (ADR-0001).
- **Vocoder IP** (AMBE/IMBE) for trunked voice — a licensing decision before C23 voice ships ([docs/04 §8.4](04-radio-engineering-and-signals-analysis.md)), not a technical spike.
- **SQLite write throughput** under dense-band detection rates — a quick benchmark folded into early build, not a headline spike (ADR-0006).
- **Feed ToS/licences** (RadioReference per-user creds, map/feed redistribution) — tracked in ADR-0010, resolved per feed.
