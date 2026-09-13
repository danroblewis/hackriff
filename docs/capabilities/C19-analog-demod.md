# C19 · analog-demod
> Layer D — Demodulate & decode · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C11, C13, C08, C15, C17 · Used by: C22, C23, C24, C25, C27, C18, C05

## Purpose
Turns a channelized analog emission into audio with **no manual mode, squelch or AGC choice**: it infers AM/NBFM/WFM/SSB/CW (and DSB-SC), opens squelch from the measured noise floor, and extracts channel attributes (CTCSS/DCS, RDS PI/PS, stereo pilot) that label and group emitters. **C19 owns RDS/RBDS decode** — it rides the FM MPX, so it is not a C22 decoder-plugins external tool (docs/06 §5). Serves workflow step 5, and step 7 via C24.

## Interface
- **In:**
  - `ChannelStream` (provisional) from C11: complex baseband at ≥2× channel BW (noise squelch needs out-of-voice spectrum; docs/04 §6.2), centre/BW, sample-time index, provenance.
  - `ParameterSet` from C13: OBW, CFO, SNR, symmetry.
  - N̂0 from C08; class distribution from C15; band/region priors from C17.
- **Out:**
  - `AudioStream` (provisional): PCM stamped with source sample time. `tools/fm_rx.py` emits 48 kHz s16le mono.
  - `SquelchEvent` open/close, used to cut per-transmission recordings in C25.
  - `ChannelAttributes`: mode + confidence, de-emphasis τ, CTCSS/DCS, stereo flag, RDS PI/PS/RT/clock, HD Radio flag, SSB carrier estimate, audio SNR.
- **Control:** SSB clarifier nudge (the only manual control), a mode override for inspection, region, max concurrent channels.

## Methods
- **Mode selection** (docs/04 §6.1): features over 100–500 ms windows.
  - WFM if OBW >120 kHz and constant envelope; confirm with the 19 kHz pilot.
  - CW if OBW <300 Hz with an on/off envelope.
  - NBFM if the envelope is constant; send to C20 if the IF histogram is discrete.
  - AM if there is a stable carrier line.
  - SSB if |P|>0.6 and OBW is 1.8–3.5 kHz.
  - Otherwise unknown, routed to C15/C20.
  - Thresholds must be trained on captures. Pre-gate with docs/04 §4.10.
- **SSB carrier:** voice low-cut edge (~200–300 Hz), snap to the 1 kHz/500 Hz raster, refine by pitch-harmonic regularity to ±10–20 Hz. >50 Hz is audibly wrong (docs/04 §6.1).
- **Squelch** (docs/04 §6.2):
  - SNR squelch vs N̂0·B: open 6 dB / close 3 dB, attack 10–20 ms, hang 300–800 ms.
  - FM noise squelch: HPF >4 kHz on the discriminator, rectify, smooth. It is gain-independent.
- **Tones** (docs/04 §6.2):
  - CTCSS: ~50 tones, 67.0–254.1 Hz. LPF <300 Hz, decimate to ~1–2 kHz, Goertzel bank; decide within 150–250 ms with persistence.
  - DCS: 134.4 bps Golay(23,12). Correlate all codewords in both polarities.
- **AGC** (docs/04 §6.3):
  - Log domain `g[n+1] = g[n] + μ(L_ref − L[n])`, attack 1–10 ms, decay 0.2–0.5 s (AM), hang 0.5–2 s (SSB).
  - FM: no pre-demod AGC, loudness normalisation after demod.
  - AM: synchronous (PLL) detector with envelope fallback.
  - Hardware gain stays with C01/C05.
- **De-emphasis/stereo/RDS** (docs/04 §6.4):
  - τ = 75 µs (Americas, South Korea) or 50 µs. NBFM: de-emphasis plus 300 Hz HPF plus 3 kHz LPF.
  - Stereo: pilot PLL, 38 kHz regeneration, mono blend at low SNR.
  - RDS: 57 kHz, 1187.5 bps differential BPSK, 26-bit blocks.
  - Exclude HD Radio sidebands (±130–200 kHz) from OBW.
- **`tools/fm_rx.py` as reference:**
  - Pipeline: 250 kHz offset tune (DC spike), 129-tap FIR at 110 kHz, ÷10 to 240 ksps, polar discriminator `angle(y[n]·conj(y[n−1]))`, 15 kHz LPF, ÷5 to 48 kHz, single-pole de-emphasis. Filter state carries across chunks.
  - A good golden output for WFM mono.
  - Lacks squelch, AGC, stereo and RDS. Runs at 2.4 Msps, below the recommended 8 Msps (docs/01 §1.2).
  - Python, so not for the real-time path.

## Platform constraints
- One half-duplex ≤20 MHz window (usable ~15–18 MHz). All simultaneous channels must sit inside it; "several simultaneous narrowband demods" is an unbenchmarked estimate (docs/01 §7.3).
- Low per-channel cost (docs/06 §2). C11's channelizer dominates.
- 8-bit ADC, no preselector. FM broadcast and pager overload; notch filters are recommended (docs/01 §1.3). Offset-tune around the DC spike.
- No TCXO (docs/01 §1.2). CFO from C13/C05 matters for SSB and CTCSS.
- Exposing cellular-band voice demod in a product needs regulatory review (47 CFR §15.121; docs/04 §1.3).

## Prior art and reuse
- **SDRangel:** AM/NFM/WFM/BFM (with RDS)/SSB/WDSP plugins. A DSP reference and a headless REST stop-gap (docs/03 §2.2). Licence: check.
- **Suscan/SigDigger:** AM/FM/USB/LSB demodulators in a library layer (docs/03 §3.4). Licence: check.
- **liquid-dsp:** AGC, PLL, filters; portable C (docs/03 §1.4). Licence: check.
- **redsea** (RDS) is a reference for RDS logic, but RDS/RBDS is owned by C19, not hosted as a C22 plugin (docs/06 §5).
- **Mayhem Audio app:** the baseline mode set (docs/01 §3.3). GPL.

## Pitfalls
- Squelch chatter near threshold (hysteresis and hang are mandatory). N̂0 contaminated by neighbours holds squelch shut.
- AGC pumping on AM fades and SSB syllables. ADC clipping masquerades as distortion.
- NBFM vs digital FSK confusion; DSB-SC vs BPSK (both square to a 2f_c line).
- Wrong SSB sideband or raster off amateur bands; wrong regional τ; HD Radio inflating the WFM OBW.
- CTCSS false detects from voice harmonics.
- Intermod products demodulate as plausible audio. Honour C05 "suspect" flags.

## Testing
- **Synthetic:** AM/NBFM/WFM (pilot + RDS group)/USB/LSB/CW/DSB-SC from tones and speech. Sweep SNR (docs/04 §12 #9 claims high robustness above ~10 dB), CFO, CTCSS/DCS codes. Assert:
  - mode accuracy vs SNR;
  - squelch latency within attack/hang;
  - CTCSS decision ≤250 ms;
  - de-emphasis response;
  - exact RDS PI/PS.
- **SigMF fixtures** (≥8 Msps, offset-tuned):
  - FM broadcast + RDS: PI/PS vs licence data; pilot within ±2 Hz.
  - NOAA Weather Radio: NBFM audio into a C22 SAME decoder.
  - Airband AM; amateur SSB.
  - `fm_rx.py` output as a WFM cross-correlation reference.
- **Live hardware:** overload, real fading for AGC, clarifier usability.

## Example use cases
Regenerated from `use-cases.yaml`:
- SIGNAL-009 — SELCAL
- SIGNAL-010 — VOLMET
- SIGNAL-011 — (analog voice/attribute demod)
- SIGNAL-012 — (analog voice/attribute demod)
- SIGNAL-013 — NDB DXing
- SIGNAL-045 — Highway advisory radio / TIS
- SIGNAL-062 — RDS/RBDS & TMC
- RESEARCH-045 — (analog demod primary)
- SIGNAL-036 — (analog demod)
- SIGNAL-037 — (analog demod)

## Open questions
- **RDS owner (resolved, docs/06 §5).** RDS/RBDS is owned by C19 (rides the FM MPX), not a C22 plugin.
- DCS needs a slicer and clock recovery: reuse C20 blocks?
- HD Radio detection: C19 flag or C16?
- Audio format, resampler, loudness target; stereo always or on demand.
- Training data for the thresholds; fusion with C15/C17 priors. Region source for τ (C06 or C17).
- Cellular-band policy (§15.121) and where content gating lives (C24?).

## Reading list
1. `docs/04 §6.1 "Discriminating AM / NBFM / WFM / SSB / CW from IQ"`
2. `docs/04 §6.2 "Automatic squelch"`
3. `docs/04 §6.3 "AGC design"` and `§6.4 "De-emphasis, stereo, RDS"`
4. `docs/04 §4.10 "Analog vs. digital discrimination (quick tests)"`
5. `tools/fm_rx.py` (~70 lines)
6. `docs/03 §2.2 "SDRangel — the most engineering-grade open-source receiver"`
