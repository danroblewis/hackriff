# C36 · gnss-observables
> Layer G — Specialised · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C01, C03, C04, C05, C08, C22, C25 · Used by: C06, C12, C27, C30

## Purpose
Runs a software GNSS receiver (GNSS-SDR class) on raw L-band IQ. It produces per-satellite C/N0, nav data, pseudorange/phase and integrity flags. This serves science (TEC, scintillation, GNSS-IR) and the attack map: local jamming and spoofing **detection**, joined with gpsjam-style context. Workflow steps: 2 (scheduled L-band dwells), 3 (C/N0 history), 4 (known constellations). Spoofing *generation* is out of scope.

**GPS L1 acquisition is an explicit, documented exception to this project's blind-first rule (CLAUDE.md: "Signals are always found by blind detection from RF data first").** L1 C/A sits **20-30 dB below the noise floor**: energy-domain blind detection (C09 CFAR, spectral kurtosis, cyclostationary search) cannot find it, so it must not be expected to. Recovering it requires **known-code correlation/despread against the published GPS PRN codes** — a known-signal pipeline that *leads* rather than *suggests*, unlike every other capability in this map. GNSS-SDR is the reference implementation for that correlation/tracking pipeline (SIGNAL-030). The blindly-detectable, on-mission half of GNSS stays blind-first as normal: **jamming/spoofing is a power-domain and PVT-consistency anomaly** (uniform C/N0 drop, floor rise, impossible PVT jumps, clock inconsistency — AWARE-002/AWARE-003), found without ever correlating against a PRN code.

## Interface
- **Inputs:** L1 dwell IQ from C03 (1575.42 MHz, `docs/04 §1.2`) at ≥8 Msps (HackRF minimum recommended, `docs/01 §1.2`), decimated in-chain. C01 provenance (gain, clip count, bias-tee, clock source). Optional L5 dwell. Time/position seed from C06.
- **Outputs (provisional objects):**
  - `GnssObservableEpoch`: per SV, C/N0, Doppler, pseudorange, carrier phase, lock time. 1 Hz logging; 10–50 Hz for scintillation (estimate).
  - `GnssNavMessage`: subframes, ephemerides, SBAS, OSNMA tags.
  - `GnssIntegrityEvent`: {jamming-suspect, spoofing-suspect, OSNMA-fail, clock-jump}, with evidence and confidence.
  - `PvtSolution`: to C06 when the SDR is the GNSS source.
- **Control:** constellation/band set, dwell request to C04, bias-tee antenna profile, integrity thresholds, raw-IF snippet recording via C25.

## Methods
- **Recommended:** wrap GNSS-SDR as a C22 plugin over IPC; don't reimplement tracking loops.
- **Acquisition:** FFT parallel code-phase search over the 1 ms C/A period. GNSS L1 is 20-30 dB below the noise floor, so energy detection fails and C09 won't "see" L1 (`docs/04 §4.9`) — see the blind-first exception in Purpose above.
- **Jamming** (AWARE-002):
  - A uniform C/N0 drop across all SVs plus a rise in the C08 L1 floor means jamming; a single-SV drop means blockage.
  - The HackRF has **no GNSS-style front-end AGC to report** (docs/06 §5) — the docs/06 C36 "front-end AGC" claim is dropped — so use in-band power versus the calibrated C08 floor as the only available "AGC" proxy.
  - Swept-tone jammers (AWARE-005) are spectral: C07/C09.
- **Spoofing tell-tales** (AWARE-003):
  - Equal power across SVs.
  - Impossible PVT jumps.
  - Clock versus C06/NTP inconsistency.
  - Broadcast ephemeris versus cached almanac (galmon style).
  - Galileo OSNMA failure (SIGNAL-031).
- **Science:** S4 and phase scintillation from high-rate observables (PROP-033); slant TEC needs dual-frequency phase (SPACE-023); SNR versus elevation for GNSS-IR (PROP-039).

## Platform constraints
- **Antenna:** an active antenna needs the bias-tee, max 50 mA at 3.0–3.3 V (`docs/01 §1.2`). Check the antenna's voltage range (unverified).
- **Clock:**
  - HackRF One has **no TCXO** (ppm unverified). This widens the Doppler search and degrades phase products.
  - Use 10 MHz CLKIN or a GPSDO (`docs/02 §1.8`). HackRF Pro has a 0.5 ppm TCXO (`docs/01 §1.7`).
- **Front end:**
  - One ≤20 MHz half-duplex window: L1 and L5 (~400 MHz apart) can't be simultaneous, so true dual-frequency TEC is `needs-other-sdr`.
  - No preselector, with Inmarsat 1525–1559 and Iridium 1616–1626.5 MHz adjacent (`docs/04 §1.2`).
- **Attention:** continuous tracking monopolises the only radio (C04).
- **Compute:** docs/06 says "medium–high continuous CPU". No benchmark in docs; measure per power mode.

## Prior art and reuse
- **GNSS-SDR:** full receiver (GPS, Galileo, SBAS). Licence: check.
- **galmon:** nav archiving and forensics (SIGNAL-032). Licence: check.
- **OSNMAlib:** OSNMA verification. Licence: check.
- **RTKLIB:** TEC (SPACE-023 ref). Licence: check.
- **gnssrefl:** GNSS-IR (PROP-039 ref). Licence: check.
- **Maintenance and licences:** status of all five is not assessed in the docs; doc 03 has no GNSS section, so GNSS-SDR, galmon and the rest have **unchecked licences — flagged for the Phase 3 licence ledger** (docs/06 §5).
- **Mayhem "GPS Sim"** (`docs/01 §3.3`): **do not inherit.**

## Pitfalls
- A handheld indoors or near the body looks like jamming. Require an all-SV drop plus a floor rise.
- A bias-tee left on into a passive or DC-shorted port. **Addressed by T-325:** `Provenance.bias_tee` is three-valued (`unknown`/`off`/`on`, never a bool — "nothing said" is not "off"), stamped by the source layer through the generic device contract, and surfaced on `/api/iqbuffer` segments and in the control panel. Reporting only: nothing auto-enables a bias tee.
- Cellular or Inmarsat IMD raises the L1 floor and creates false jamming flags. Cross-check C05 gain-step tests.
- DC spike at band centre (`docs/01 §1.3`): tune L1 off-centre.
- Clock drift without a time aid produces false "spoofing" clock jumps.
- Time-sliced dwells break carrier-phase continuity needed for scintillation and TEC.
- OSNMA needs accurate time and a cached public key offline (C29).

## Testing
- **Fixtures:** own L1 SigMF captures (≥8 Msps, 60–300 s) with a surveyed antenna point and a reference u-blox log. Public GNSS-SDR samples if licence allows (existence unverified).
- **Assertions:** ≥4 SVs acquired; C/N0 within ±3 dB of the reference (estimate); PVT error versus survey point; nav CRC pass rate; OSNMA status on a known-good capture.
- **Synthetic jamming:** add noise and a chirp to recorded IQ at stepped J/S. The flag must fire; single-SV attenuation must not.
- **Spoofing logic:** test on mocked observables (equal power, jumps, clock steps). **Build no GNSS RF generator.** Any third-party simulator output stays file-only, never routed to C37.
- **Live hardware:** antenna/bias-tee compatibility, TTFF, CPU and thermals per power mode.

## Built so far (T-274, opening M5 — see [ADR-0018](../adr/0018-gnss-known-code-exception.md))

`crates/hk-gnss`, deliberately **not** wired into `hk-pipeline` (nothing schedules an L1 dwell yet).

**Built and tested offline:** the GPS L1 C/A Gold-code generator and codebook (verified by the Gold-code three-valued correlation property {−1, −65, 63} against a 1023 peak, and by code balance); FFT parallel code-phase acquisition over the Doppler × PRN grid; observable epochs and the S4 index; blind jamming assessment and spoofing tell-tales on mocked observables.

**The exception, measured not asserted.** On one piece of synthetic IQ at −20 dB SNR in 2.046 MHz, the peak periodogram excursion above median is 6.19 dB with the satellite present and 6.16 dB with it removed — a 0.02 dB difference, so energy detection has nothing to threshold. Known-code correlation on that same IQ recovers PRN 11 at 511.00 chips (truth 511), 1250 Hz Doppler (truth 1180, one grid step), peak/mean 17.2, C/N0 estimated 42.1 dB-Hz against an analytic 43.1.

**How the exception is confined:** `hk-detect` (and `hk-core`/`hk-dsp`/`hk-estimate`) depend on neither `hk-gnss` nor `hk-context`, so a `PrnCodebook` is un-nameable inside the detector; `tests/blind_path_boundary.rs` fails if that edge is ever added. The jamming half stays blind — `assess_jamming` takes receiver observables as an `Option` and still flags jamming with `None`.

**Not built by T-274:** tracking loops, nav decode, ephemeris, PVT. Acquisition without tracking cannot produce a fix, and none is claimed from it.

**Ephemeris forensics (T-324, SIGNAL-032):** `hk_context::ephemeris` parses GPS LNAV ephemerides from RINEX 2/3 nav (the receiver's own log, e.g. GNSS-SDR's, and the IGS BRDC reference share one parser), evaluates them with IS-GPS-200 Table 20-IV, and compares them against cached IGS SP3 precise orbits, falling back to reference broadcast ephemerides (`feeds::gnss_orbits`, C29). A wrong orbit (spoofing, bad upload) and a clock step are flagged separately; an absent reference reads as not-yet-fetched, never as agreement. It compares navigation *data*, so it cannot seed a detector (ADR-0018 unaffected). Tested on a synthetic constellation only; not yet wired to C30 or to the GNSS-SDR plugin's nav output.

## GNSS-SDR plugin (T-323)

The track-and-fix half comes from **GNSS-SDR wrapped as a C22 plugin**, as Methods recommends, not from reimplemented loops.
- **Pieces:** `plugins/gnss-sdr/manifest.json` → `hk-plugin-gnss-sdr` (bin of `hk-gnss`) → `gnss-sdr --config_file=…`.
  - The wrapper spools each contiguous `ci8` L1 dwell to a scratch file: `--dwell-s`, default 60 s. A gap or drop marker closes the dwell early, and one shorter than `--min-dwell-s` (default 36 s) is discarded.
  - It then runs GNSS-SDR over that file: File_Signal_Source `ibyte`, GPS L1 C/A PCPS acquisition, DLL/PLL tracking, telemetry decode, RTKLIB single-point PVT.
  - It reads back the RINEX 3 observations and NMEA that GNSS-SDR writes (`hk_gnss::receiver`).
- **Output, schema `hackriff.gnss/1`:** one `gnss-sdr-epoch` decode per observation epoch, carrying a `GnssObservableEpoch` with C/N0, Doppler, pseudorange, carrier phase, elevation and the fix. It also emits one `gnss-sdr-dwell` summary per dwell, always, so "ran and saw nothing" is distinct from "never ran".
  - `lock_evidence` gives AWARE-002 **real per-SV C/N0** loss, which corroborates the blind floor rise; `assess_jamming` keeps working with `None`.
  - `s4_by_prn` gives PROP-033 S4 from tracked C/N0. **This is an approximation:** it uses 10 Hz C/N0 rather than detrended 50 Hz correlator intensity.
- **Licence boundary (ADR-0010):** GPL-3.0-or-later, exec'd only. `tests/gnss_sdr_boundary.rs` fails on a build script, `links`, FFI, a `-sys`/gnss-sdr dependency, a missing ledger row, or a wrapper that emits `identity`/`annotation`.
- **Evidence, never detection:** the wrapper emits no identity, so the ingest upserts no Emitter per PRN, and no annotation, so nothing targets a detection. `tests/gnss_sdr_plugin.rs` asserts `emitters_upserted == 0` through the real plugin host.
- **Time:** GPS time is converted to UTC with the RINEX `LEAP SECONDS` (default 18 s). A decode's `sample_index` is the **dwell start**, because RINEX has no input sample counter; the exact receiver time is `epoch.t`.
- **Unverified:** no GNSS-SDR install exists on the dev Mac (there is no Homebrew formula). The config keys follow GNSS-SDR's documentation and are unconfirmed. Tests drive a spec-shaped stand-in (`hk-fake-gnss-sdr`), and `real_gnss_sdr_accepts_the_generated_config` runs only when `gnss-sdr` is on `PATH`. Tracking accuracy, TTFF and C/N0 against a reference need a real L1 capture from an active antenna.
- **Not yet:** routing the plugin into `hk-pipeline`'s scheduled L1 dwell (T-322 runs acquisition only), SBAS/WAAS, OSNMA, and per-epoch sample alignment (it needs GNSS-SDR's monitor stream).

**Unverified:** every sensitivity and C/N0 claim rests on synthetic IQ. Settling it needs an active GNSS antenna on the bias-tee and a real L1 capture (user-triggered).

## Example use cases
Regenerated from `use-cases.yaml`:
- AWARE-002 — Local GNSS C/N0 watchdog (in-band-power proxy, no true AGC)
- AWARE-003 — Spoofing tell-tales detector
- SIGNAL-030 — GPS L1 C/A + SBAS/WAAS raw processing
- SIGNAL-031 — Galileo OSNMA authentication
- SIGNAL-032 — GNSS constellation forensics
- PROP-033 — GNSS scintillation with SDR
- SPACE-007 — Solar radio bursts vs GNSS
- SPACE-023 — GNSS total electron content
- PROP-039 — GNSS-IR snow depth
- PROP-080 — Antenna patterns from satellite passes

## Open questions
- **C06 source:** should it use a dedicated GNSS module so C36 runs only on scheduled dwells? This is an attention-budget ADR.
- **"Front-end AGC" (resolved, docs/06 §5):** the HackRF has none; C36 reports an in-band-power / noise-floor proxy instead.
- **Dual-frequency products (resolved, docs/06 §5):** dual-frequency TEC cannot fit one 20 MHz window on a single HackRF, so it is `needs-other-sdr`.
- **Licences (resolved, docs/06 §5):** GNSS-SDR/galmon/etc. licences are unchecked and go to the Phase 3 licence ledger.
- **AWARE-005:** confirm it maps to C09/C10 first, with C36 secondary.
- **GNSS-IR:** docs/06 C36 omits it (SNR versus elevation, no nav solution).
- **GNSS-SDR:** process boundary, licence isolation, and CPU fit alongside C07/C11.

## Reading list
1. `docs/05 §3 "GNSS Jamming, Spoofing & Navigation Integrity"` — target behaviours and refs.
2. `docs/04 §4.9 "DSSS detection"` — correlation, not energy.
3. `docs/01 §1.2 "Specifications"` — bias-tee, clock, sample-rate limits.
4. `docs/05 §2 "Satellites & GNSS: ionosphere and troposphere"` — science products.
5. `docs/02 §1.8 "Clock accuracy (TCXO/OCXO/GPSDO)"`.
