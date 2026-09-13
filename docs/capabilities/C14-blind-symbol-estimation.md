# C14 · blind-symbol-estimation
> Layer C — Characterize · Status: draft (taxonomy draft 2026-09-13) · Depends on: C11, C13 (optional family hint from C15) · Used by: C15, C18, C20, C21, C23

## Purpose
Estimates symbol-level structure blind: symbol rate, FSK deviation and index, modulation order, roll-off, GFSK BT and cycle frequencies. These drive blind demodulation (workflow step 6) and are the strongest fingerprint fields: symbol rate to ±1% plus deviation plus sync word identifies most LMR and ISM protocols (docs/04 §7.6). docs/06 names this a Phase 4 spike.

## Interface
- **Inputs** (provisional names):
  - `ChannelSnippet`, coarse-CFO-corrected by C13.
  - `ParameterSet` (OBW, SNR, sample rate).
  - Optional family hint.
  - `mode ∈ {fast, deep}`; deep = SSCA/FAM on demand.
- **Output: `SymbolParameters`.** Each field carries value, uncertainty and method.
  - `symbol_rate_bd` with top-k candidates (harmonics are ambiguous); `sps`.
  - FSK: `levels`, `peak_positions_hz`, `deviation_hz`, `mod_index_h`.
  - `order_hypothesis`, tagged `pre_sync`/`post_sync`.
  - `roll_off`, `gfsk_bt`; OOK pulse/gap histograms.
  - Deep mode: `cycle_frequencies[]`.
- **Unknown handling:**
  - `symbol_rate: null` with reason `no_cyclic_line`, `too_short` or `low_snr`.
  - `analog_likely` when there is no line and the instantaneous-frequency histogram is continuous.
- **Refinement.** C20's timing loop feeds back its frequency term, reaching ppm level (docs/04 §4.4).
- **Config:** rate range bounded by OBW (−3 dB bandwidth ≈ Rs for linear modulations, docs/04 §4.5); delay D ≈ T/2; SSCA N/N′; coherence threshold.

## Methods
- **Symbol rate** (docs/04 §4.4):
  - **Linear:** |x|² spectral line at Rs, strength roughly ∝ α. Delay-and-multiply `x[n]x*[n−D]` strengthens it.
  - **FSK:** Haar-wavelet transient method, or the run-length histogram fundamental of the sliced instantaneous frequency.
  - **OOK:** envelope pulse/gap histograms, as rtl_433 `-A` does.
  - **Preamble:** `1010…` gives a tone at Rs/2 (docs/04 §7.3).
- **Deep mode:**
  - SSCA/FAM with spectral coherence.
  - Non-conjugate α = k/T; conjugate 2f_c + k/T for BPSK/ASK; 4f_c for QPSK/QAM; MSK at 2f_c ± Rs/2 (docs/04 §5.2).
  - Fine on one emission of a few × 10⁴ samples.
- **Deviation** (docs/04 §4.6):
  - Discriminator `f_i = fs/(2π)·arg(x[n]x*[n−1])`; histogram with L peaks.
  - Δf = half the outer spacing; h = 2Δf/Rs.
  - Peak positions often identify the protocol: P25 ±600/±1800 Hz, DMR ±648/±1944 Hz.
- **Order, roll-off, BT** (docs/04 §4.5):
  - Pre-sync: cumulant groups (shared with C15). Post-sync: k-means over M, chosen by BIC.
  - α ≈ BW_null/Rs − 1.
  - BT from the instantaneous-frequency eye.
- **Refinement** (docs/04 §7.2): Gardner for PSK/QAM; Oerder–Meyr for bursts under ~50 symbols.

## Platform constraints
- **Sample clock.** Error scales symbol rates; it shares the LO crystal, so one C05 ppm estimate fixes both (docs/04 §10.1). ±1% fingerprinting tolerates it.
- **8-bit clipping** adds envelope harmonics, which become false cyclic lines. Honour clip and IMD flags (docs/04 §10.4).
- **Compute.** Spectral-line methods: μs–ms per event on ARM. SSCA only for unknown or hard emissions (docs/04 §5.5). A CuPy port is plausible (estimate).
- **Dwell, not sweep.** A full sweep catches a 5 ms burst with ~0.7% probability per occurrence (docs/04 §3.8), so C14 needs dwell pre-trigger capture (C03).

## Prior art and reuse
- **SigDigger/Suscan:** baud via symbol autocorrelation; ASK/FSK/PSK inspectors. docs/06's reference. Licence: check. Single maintainer.
- **URH:** automatic modulation type and samples/symbol. GPLv3, archived 2026-03-29: borrow ideas.
- **rtl_433 `-A`:** timing histograms, slicer guesses. Licence: check. Active.
- **GNU Radio `symbol_sync`:** refinement. GPL.
- **CSP blog (Spooner):** SSCA/FAM recipes.
- **inspectrum:** manual symbol-rate cursors.

## Pitfalls
- A small roll-off hides the |x|² line; constant-envelope FSK defeats envelope methods.
- Harmonic ambiguity (Rs vs 2Rs); Manchester changes the apparent rate, so let C21 resolve line code.
- False lines from tones: WFM 19 kHz pilot, CTCSS 67–254 Hz (docs/04 §4.10, §6.2).
- Long NRZ runs and few symbols break run-length estimates.
- Residual CFO and unbalanced data skew deviation histograms.
- Pre-sync high-order QAM needs thousands to tens of thousands of symbols (docs/04 §5.2).
- HF multipath smears transitions.

## Testing
- **Synthetic:**
  - PSK/QAM with α ∈ {0.1…0.5}, sps 2–16.
  - 2/4-FSK and GFSK (BT 0.3/0.5, h 0.5/1); OOK PWM/Manchester.
  - 20–2000 symbols; SNR −5…+25 dB.
  - ppm clock offset; 8-bit quantisation.
- **Metrics:** Rs relative error (goal ±1%, docs/04 §7.6), correct-harmonic rate, deviation/h/α error, CPU time.
- **SigMF fixtures** (docs/04 §1.2, §4.6):
  - AIS GMSK 9600 bps, h = 0.5; P25 C4FM 4800 sym/s; DMR.
  - RS41 GFSK 4800 bps; NOAA SAME AFSK 520.83 bps; RDS 1187.5 bps.
  - Meteor-M LRPT QPSK ~72 ksym/s; ADS-B PPM 1 Mbps.
  - OOK sensors verified by rtl_433.
  - POCSAG: parameters only, never content (docs/04 §1.3).
  - Decoder CRC pass = ground truth.
- **Live only:** short-burst intercept; temperature drift.

## Example use cases
*Provisional until docs/06 §3 mapping (cross-checked against use-cases.yaml, 2026-09-13).*
- AWARE-036 — Unknown burst reverse-engineering triage
- RESEARCH-001 — Blind ISM device RE with URH
- RESEARCH-002 — rtl_433 flex decoder
- RESEARCH-003 — Bit-level dissection in inspectrum
- SIGNAL-069 — STANAG 4285 modems
- SIGNAL-047 — Keyless entry fingerprinting
- SIGNAL-043 — PTC 220 MHz
- RESEARCH-064 — Reverse-engineered LoRa PHY decode

## Open questions
- **Spike.** Which estimator mix wins on HackRF captures? SSCA on CPU or CuPy?
- **Mapping undercount.** The mapping makes C14 primary for no use case and lists it in only 9, yet every unknown-digital decode needs it. Priority should come from its dependents (C20/C21/C18), not its count.
- **Order in two places.** Modulation order is listed in both C14 and C15; post-sync order needs C20. The C14 ⇄ C15 ⇄ C20 loop is missing from §2.1.
- **CSS/LoRa unassigned.** Chirp parameters (SF, BW) belong to neither C14 nor C16.
- **Shared CSP engine.** C15 and C16 also need cyclostationary features; share one engine.
- **URH code.** Port the archived GPLv3 code or reimplement?

## Reading list
1. docs/04 §4.4 "Symbol-rate estimation"
2. docs/04 §4.6 "FSK deviation and modulation index"
3. docs/04 §4.5 "Modulation order, pulse shape, roll-off"
4. docs/04 §7.2 "Clock (symbol timing) recovery"
5. docs/03 §3.4 "Protocol reverse engineering and signal inspection"
6. docs/04 §7.5 "How existing tools approach it"
