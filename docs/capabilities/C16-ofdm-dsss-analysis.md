# C16 · ofdm-dsss-analysis
> Layer C — Characterize · Status: draft (taxonomy draft 2026-09-13) · Depends on: C11, C13, C05 · Used by: C15, C22, C27, C18, C36, C35

## Purpose
Handles the two families generic estimators treat badly:
- **OFDM** (LTE/NR, Wi-Fi, DVB-T, DAB, ATSC 3.0): estimates subcarrier spacing, CP length, symbol period, fractional CFO and active subcarriers.
- **DSSS** (GNSS, 802.11b, 802.15.4): detects it, including below the noise floor where energy detection fails, and identifies it by known-code correlation.

This cheaply moves wideband infrastructure out of the "unknown" pile (workflow step 4) and feeds C15's family branch.

## Interface
- **Inputs** (provisional names):
  - `ChannelSnippet` spanning many symbols or code periods.
  - `ParameterSet` (OBW, coarse CFO, SNR).
  - Optional band hypotheses (C17).
  - Known-code library (GPS C/A, Barker, …).
- **Output: `OfdmParameters`.**
  - `detected`, `score`, `subcarrier_spacing_hz`, `cp_len_s`, `Ts_s`.
  - `fractional_cfo_hz`, `n_active_subcarriers`.
  - `standard_candidates[]` with likelihoods.
- **Output: `DsssParameters`.** `detected`, `method`, `chip_rate_hz`, `code_period_s`, `code_match` (code id, phase, Doppler bin).
- **Unknown handling:**
  - `none` when neither structure is present, as docs/06 requires.
  - `undetermined` (`too_short`, `low_snr`, `partial_band`) when the test couldn't run properly.
- **Config:** lag search range, snippet duration, code set, Pfa.

## Methods
- **CP correlation** (docs/04 §4.8):
  - `γ(m,D) = Σ x[k]·x*[k+D]`; the normalised peak gives D̂ = N_FFT.
  - Tu = D̂/fs, Δf = 1/Tu.
  - CP length from the plateau width of |γ(m,D̂)|; Ts from its periodicity.
  - Fractional CFO = −∠γ/(2πTu) (van de Beek 1997).
  - Active subcarriers ≈ OBW/Δf; cyclic features at α = k/Ts.
- **Standards:** LTE 15 kHz, NR 15·2^μ kHz, Wi-Fi 312.5 kHz, DVB-T 1116/4464 Hz (docs/04 §4.8). Band context from docs/04 §1.2.
- **LTE carrier:** PSS/SSS (Zadoff-Chu) acquisition, as in LTE-Cell-Scanner. Base stations hold ±0.05 ppm, so this also serves C05 (docs/04 §10.1). Stop at broadcast sync.
- **DSSS** (docs/04 §4.9):
  - Autocorrelation-fluctuation/covariance at lags k·T_chip or the code period (GPS C/A: 1 ms).
  - Cyclic feature at α = R_c.
  - FFT-based parallel code-phase search.
  - sinc² lobe of width 2R_c when SNR allows.
  - The code search suits the GPU (estimate).

## Platform constraints
- **Bandwidth.** 20 Msps, ~15–18 MHz usable (docs/01 §7.3). 20 MHz LTE/NR and Wi-Fi are captured only partially; Wi-Fi 6E is above 6 GHz (docs/04 §1.2). Partial-band CP correlation should weaken rather than fail (estimate; spike).
- **Dynamic range.** 8-bit, no preselector. Cellular downlinks set gain and create IMD (docs/02 §1.7). Instantaneous dynamic range is set by the strongest in-window signal, which limits below-noise DSSS detection (docs/02 §1.2).
- **Frequency error.** 1 ppm at 1.575 GHz ≈ 1.6 kHz of extra GNSS search (derived from docs/04 §10.1). GNSS needs an active antenna on bias-tee.
- **Compute.** Low–medium per event (docs/06).

## Prior art and reuse
- **gr-inspector:** OFDM estimator (RESEARCH-005). Stale (GNU Radio 3.8). Licence: check.
- **LTE-Cell-Scanner:** PSS/SSS with large initial offsets. Licence: check.
- **srsRAN:** LTE sync reference. Licence: check; legal fence applies.
- **GNSS-SDR:** acquisition; share the core with C36. Licence: check.
- **TorchSig 2.2:** 802.11a/Zigbee/BLE synthetic signals. MIT.
- **PySDR:** OFDM chapter (RESEARCH-077).

## Pitfalls
- **Delay spread** smears the CP plateau, so CP estimates run short and CFO gets noisier.
- **TDD gaps and short Wi-Fi packets** leave few symbols; gate to active time.
- **Contiguous carriers** in one channel inflate OBW and the subcarrier count.
- **ATSC 1.0 is 8-VSB, not OFDM** (docs/04 §1.2); use it as a negative control.
- **Periodic spurs at the code-period lag** mimic DSSS; apply C05's spur map.
- **Doppler plus ppm** enlarges the code search.
- **Legal fence.** Cellular content is off-limits even when decodable (docs/04 §1.3). C16 stops at PHY parameters and broadcast sync/identity. PDCCH/RNTI work (AWARE-018/019, RESEARCH-024/025) needs legal review and is out of this card.

## Testing
- **Synthetic:**
  - OFDM with Δf ∈ {15 kHz, 30 kHz, 312.5 kHz, 1116 Hz}, several CP ratios, fractional CFO, tapped-delay multipath, SNR −5…+25 dB, 8-bit quantisation.
  - DSSS: GPS-like 1 ms code, Barker-coded, and 2 Mchip/s O-QPSK, SNR below 0 dB.
- **Metrics:**
  - Δf relative error, CP error in samples, CFO error.
  - DSSS ROC (Pd vs Pfa by SNR); standard-identification accuracy.
  - No docs targets.
- **SigMF fixtures:**
  - LTE downlink (15 kHz).
  - ATSC 3.0 where deployed, with ATSC 1.0 as the negative control.
  - DVB-T/DAB outside the US.
  - Wi-Fi 2.4 GHz (partial band).
  - 802.15.4 at 2405 + 5(k−11) MHz.
  - GPS L1 1575.42 MHz with an active antenna.
- **Live:** bias-tee GNSS; dense cellular IMD.

## Example use cases
*Provisional until docs/06 §3 mapping (cross-checked against use-cases.yaml, 2026-09-13).*
- SIGNAL-066 — ATSC 3.0 bootstrap & wake-up bits
- PROP-075 — LTE navigation
- RESEARCH-005 — Blind signal detection with gr-inspector
- AWARE-041 — PSD-based technology classifier
- AWARE-045 — CBRS/shared-band incumbent activity sensing
- SIGNAL-048 — Cellular broadcast metadata & calibration
- SIGNAL-065 — DRM digital shortwave
- AWARE-021 — DJI DroneID decoding

## Open questions
- **Fenced items in the mapping.** C16 is primary for RESEARCH-022 (ReVoLTE: decrypting others' calls), RESEARCH-024/025 and Wi-Fi CSI sensing (PROP-065–067, out-of-scope). Under the CLAUDE.md guardrails these must not shape C16's scope.
- **Shared acquisition.** Known-code search overlaps C36 (GNSS); PSS search overlaps C05. Share one core?
- **CSS/LoRa and FHSS** have no parameter estimator in docs/06. Extend C16 to "spread spectrum incl. CSS", or assign to C14.
- **Missing edge.** §2.1 omits C16 → C15.
- **LTE/NR scope.** Which broadcast fields (MIB/SIB, cell ID) are in scope? ADR.
- **Standards table.** Owned by C16 or C17?
- **Partial-band OFDM** accuracy (spike).

## Reading list
1. docs/04 §4.8 "OFDM parameter estimation"
2. docs/04 §4.9 "DSSS detection"
3. docs/04 §1.3 "Legal considerations (US; not legal advice)"
4. docs/04 §1.2 "What lives where (HF to ~6 GHz, US focus)"
5. docs/04 §10.1 "Frequency calibration"
6. docs/01 §7.3 "HackRF as a front end for Pi 5 / Orin Nano (± FPGA)"
