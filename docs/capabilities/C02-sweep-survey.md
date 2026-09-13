# C02 · sweep-survey
> Layer A — Acquire · Status: draft (taxonomy draft 2026-09-13) · Depends on: C01, C04, C05 · Used by: C04, C08, C09, C12, C26, C33, C39

## Purpose
Wideband power-spectrum survey over 1 MHz–6 GHz using firmware-driven retuning. It answers *where*: persistent emitters, occupancy, change. It does not find short bursts. It is the discovery half of sweep/dwell and serves workflow steps 1–2, plus step 3 via C26.

## Interface
- **Input (provisional `SweepPlan`):** spans (Hz), bin width (2,445 Hz–5 MHz), per-band gain table, Opera Cake frequency→port map, sweep count or continuous, intent label.
- **Output (provisional `SweepFrame`):** one row per completed sweep with bin frequencies, float32 dB per bin, per-slice timestamps, and per-slice provenance (gain, port/filter, clip). It also carries the calibration version for later dBFS→dBm mapping.
  - Derived estimate: 0–6 GHz at 100 kHz bins is 60,000 bins ≈ 240 kB/row, ~0.3 MB/s at ~1.3 sweeps/s.
- **Control:** start/stop/pause. C04 can preempt for a dwell and resume.

## Methods
- **`hackrf_sweep` mechanics** (docs/01 §1.6 "Firmware, `hackrf_sweep`, and host tools"):
  - Firmware steps a frequency list, so there is no USB request per step.
  - Fixed 20 Msps and 15 MHz filter; `TUNE_STEP` 20 MHz, `OFFSET` 7.5 MHz.
  - Each tune yields two 5 MHz slices clear of the DC spike and skirts. Slices arrive out of order.
- **Reuse:** the `hackrf_sweeper` library, or libhackrf `hackrf_init_sweep` directly (docs/03 §3.1 "Wideband sweep / survey").
- **Host FFT:** N ≈ 20 Msps / bin width (derived). Hann Welch; RBW = 1.5 bins × fs/N (docs/04 §3.1 "PSD estimation").
- **Per-bin accumulators:** max-hold, mean, SK. These feed candidate selection (docs/04 §3.8 "Sweep-based survey vs. real-time IBW").
- **Spec formulas for survey claims** (docs/04 §3.8):
  - T_R = N_steps·(T_s + T_d), where N_steps = ⌈Span/IBW⌉.
  - P_POI ≈ min(1, (τ + T_d)/T_R).
  - P_≥1 = 1 − (1 − P_POI)^(r·T_obs).
- **Occupancy RBW:** no coarser than the narrowest channel spacing. If RBW < OBW, lower the threshold by 10·log10(OBW/RBW) (docs/04 §3.9 "Occupancy statistics methodology (ITU-R SM.1880 / SM.2256)").
- **Gain:** per-band tables from C05, learned over time, no clipping (docs/04 §10.4 "Dynamic range management").

## Platform constraints
- Advertised ~8 GHz/s: 0–6 GHz in ~0.75 s (docs/01 §1.6).
  - Per-step figures disagree: docs/01 derives ~2.5 ms/step; docs/04 §3.8 gives ~600 steps × 1.25 ms with ~820 µs lost per retune.
  - Measure on target.
- Each chunk is seen for only a few ms per sweep. A 5 ms burst in 902–928 MHz has P_POI ≈ 0.7% per occurrence (docs/04 §3.8).
- Uncalibrated dB, 8-bit, residual spurs (docs/01 §1.6).
- Opera Cake covers only 1 MHz–4 GHz (docs/02 §5 "Antennas and RF front-end accessories").
- About one host core for continuous 20 Msps FFT (docs/06 C02 row).
- Sweeping occupies the single half-duplex radio; no concurrent dwell (docs/01 §7.3).

## Prior art and reuse
- **`hackrf_sweep` / `hackrf_sweeper`:** stitching and interleave logic. Active; licence: check.
- **Spectre (HB9TF):** long-term sweep storage in SQLite, waterfall rendering, filters (docs/03 §3.1). Active; licence: check.
- **NTIA SCOS Sensor:** task "actions" plus SigMF metadata (docs/03 §3.1). Licence: check.
- **QSpectrumAnalyzer, rtl_power + heatmap.py:** UX references. Low maintenance.
- **Mayhem Looking Glass/Search:** anti-patterns. Slow and at most 80 MHz span (docs/01 §3.4 "Spectrum and "exploration" apps and their limits").
- **`tools/sweep_plot.py`:** parsing lessons below. Example: `hackrf_sweep -f 88:108 -w 50000 -N 30 -l 24 -g 20`.

## Pitfalls
- **Settling:** the first sweep reads low while gain and PLL settle. Discard it (`sweep_plot.py` does).
- **Row order:** rows within a sweep are unsorted. A sweep restarts at the lowest `hz_low`.
- **Partial sweeps:** drop or mark sweeps cut off by stop or preemption.
- **Timestamps:** the CLI gives one timestamp per sweep. Stamp per slice for history and correlation.
- **Urban IMD:** ghost WFM carriers grow with gain (docs/02 §1.7 "Overload and intermodulation in urban RF"). Gain-step and retune tests come from C05.
- **Out-of-band energy:** harmonic mixing and images appear without a preselector (docs/02 §1.5 "DC offset, IQ imbalance, image rejection, harmonic responses").
- **Absence ≠ no emitter:** label occupancy with revisit time and POI.
- **False novelty:** gain-table changes between sweeps look like spectrum change. Compare provenance.

## Testing
- **Parser/stitcher:** recorded CSV and binary output with shuffled slices. Assert a monotonic axis, no duplicate bins, first-sweep discard and partial-sweep drop.
- **Synthetic:** per-step IQ with known tones. Assert peaks within ±1 bin and ±1 dB (estimate target). A bursty emitter model should match the empirical POI to the formula.
- **Fixtures:**
  - An 88–108 MHz FM sweep (local stations as truth).
  - A full 0–6 GHz city sweep (spur/IMD examples).
  - A terminated-input sweep (C05 spur mask).
- **Live:** full-range rate on the Jetson vs ~0.75 s; Opera Cake switching; preempt/resume latency.

## Example use cases
Provisional until docs/06 §3 mapping.
- RESEARCH-050 — SDR as spectrum analyzer / power survey
- AWARE-031 — Long-term noise-floor trend logger
- AWARE-042 — Duty-cycle and occupancy statistics
- AWARE-033 — Radio-quiet-zone style site survey
- SPACE-003 — e-CALLISTO solar burst spectrograms
- SPACE-050 — Natural radio noise floor survey
- RESEARCH-057 — EMC pre-compliance scanning
- SPACE-078 — Rediscovering lost space-weather spacecraft

## Open questions
- Add a host-driven software sweep for non-HackRF sources? docs/06 names only the firmware mode.
- Who owns per-bin SK for sweep rows: C02 or C07? docs/06 gives SK to C07.
- SPACE-003 needs a stable cadence, not only speed. Is a "science sweep" profile a C02 mode or a C33 concern?
- Confirm sweeps stay 8-bit on the Pro, since 16-bit needs ≥16× decimation.
- docs/06 §2.1 doesn't show C02 → C04, although the scheduler's discovery input is sweep statistics.

## Reading list
1. docs/04 §3.8 "Sweep-based survey vs. real-time IBW"
2. docs/01 §1.6 "Firmware, `hackrf_sweep`, and host tools"
3. docs/04 §3.9 "Occupancy statistics methodology (ITU-R SM.1880 / SM.2256)"
4. docs/03 §3.1 "Wideband sweep / survey"
5. docs/02 §1.6 "Preselection filters: why they matter for wideband surveys"
6. docs/04 §10.4 "Dynamic range management"
