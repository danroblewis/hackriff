# Capture 2026-09-15 — FM band around 100.8 MHz (live HackRF)

Preserved live off-air capture from the dev/demo HackRF, saved before the staging
server's data dir was recycled. Now an annotated blind-detection acceptance fixture
for the 88–108 MHz region as sampled at this site
(`tests/e2e/tests/acceptance/fm_band_2026_09_15.rs`, use cases **SIGNAL-062** and
**AWARE-053**).

## Files

- `iq.sigmf-data` — raw IQ, **ci8**, 216,006,656 bytes (216 MB), in **Git LFS**.
  Too large for the ≤ 25 MB committed-fixture cap in `fixtures/README.md`, so it is
  deliberately **not** listed in `fixtures/manifest.json`; the acceptance test skips
  when the LFS data is not fetched (`HK_REQUIRE_FIXTURES=1` fails instead).
- `iq.sigmf-meta` — conforming SigMF with measured `hackriff:provenance` and the
  measured `hackriff:truth` annotations described below.
- `iq.hackriff-sidecar.json` — the original `hackriff-output-sidecar` v1.0 metadata.

## Capture provenance (from the sidecar — this is measured truth, keep it)

| field | value |
|---|---|
| center | 100.800 MHz |
| sample rate | 2.400 Msps |
| span recorded | 99.600 – 102.000 MHz (requested band; single tune, no retune) |
| duration | 45.0014 s (108,003,328 samples; 2026-09-15T23:41:01.975Z → 23:41:46.977Z) |
| device | HackRF One serial …d2b861dc263bc293, fw 2026.01.3, board rev 0, libhackrf 0.9.2 |
| gain | LNA 32 dB, VGA 30 dB, **amp ON** |
| baseband filter | 1.75 MHz |
| antenna | unknown port |
| integrity | 0 dropped records, 0 lost samples, 1648 records |
| overload | measured false: 0 of 20,000,000 sampled ci8 components at the rail |

**The analogue passband is narrower than the recording.** With a 1.75 MHz baseband
filter at a 100.8 MHz centre, only **99.925 – 101.675 MHz** is inside the passband,
even though 99.6 – 102.0 MHz was recorded. Emissions outside it are attenuated and
their measured level and bandwidth are truncated by the filter skirt, not by the
emission. This is why the 99.6999 MHz station below reads weak and narrow.

## Measured truth (analysis pass, T-289, 2026-09-16)

Written into `iq.sigmf-meta` as `hackriff:truth` annotations, each carrying the
measurement that justifies it. The truth is **hidden from the system under test**:
the blind harness strips annotations and the description before the mock SDR serves
the recording, and the test reads the truth only afterwards, to match against what
the run produced.

How it was measured:

- **`hk replay`** over the whole 45 s through the composed pipeline (mock SDR device):
  1857 detections, 25 tracks, 17 emitters, 3 of them Confirmed.
- **Continuous emissions**: mean periodogram over the whole capture, 8192-point Hann
  FFT (RBW 292.97 Hz), against a 1501-bin (440 kHz) running-median baseline that
  absorbs both the noise floor and the filter skirt.
- **Bursts**: every 27.31 ms time bin (1648 of them) against the *per-frequency median
  over time*, so an emission that is on for a minority of the capture is measured
  against its own quiet state. 441 clusters above 8 dB were enumerated; all are
  attributable to the emissions and artefacts below.
- **Modulation**: each candidate mixed to baseband, filtered, decimated and
  FM-discriminated over all 45 s; a 19 kHz stereo pilot tested by Welch (32768-point,
  12.21 Hz resolution) against the local median, with 100.300 MHz as the negative control.

### Emissions

| centre | 99 % OBW | SNR | presence | what the measurement says |
|---|---|---|---|---|
| **101.298663 MHz** | 127.7 kHz | 13.70 dB | 1648/1648 bins | WFM broadcast. 19 kHz pilot **+35.5 dB**; RDS **PI 1694** decoded CRC-valid (PTY 7, groups 0A/2A/12A). On the 101.3 MHz raster. Same station as the 2026-09-13 fixture, 2.5 days earlier. |
| **99.699944 MHz** | 70.3 kHz | 4.72 dB | 889/1648 bins | WFM broadcast. 19 kHz pilot **+8.7 dB**, in the same 12.21 Hz bin as the strong station's. Weak and narrow because it is 1.10 MHz off centre, outside the filter passband. Not intermittent: detected in every 5 s of the capture. |
| **100.465339 MHz** | 28.1 kHz | 7.02 dB | 1646/1648 bins | **Unidentified**, and anticipated by nobody. Continuous and very steady. No 19 kHz pilot (+0.2 dB), so not stereo FM; its IQ mirror at 101.1347 MHz is empty, so not a receiver image. Modulation unverified. |

### Receiver artefacts (flagged, never catalogued as emitters)

| centre | what the measurement says |
|---|---|
| **99.999572 MHz** | 10 MHz reference harmonic (n = 10). 586 Hz wide, steady to **0.93 Hz RMS** over 20 one-second blocks against the receiver's own clock — which is what a tone made from that clock must do, and an off-air carrier read through an uncalibrated −6.8 ppm clock would not. The detector independently flags all 713 detections here `spur_reason = ref-harmonic`, and the 2026-09-13 capture at the same tuner centre measured the same line at 99.999609 MHz. Not retune-verified here (single tune, no retune). |
| **100.800000 MHz** | DC offset at the tuner centre; all 45 detections flagged `spur_reason = dc`. |

### Measured silent (recorded in the scenario truth's `measured_absent`)

| window | what the measurement says |
|---|---|
| **100.300 MHz ± 10 kHz** | **Nothing, for all 45 s.** SNR −11.5 dB, no continuous excess, and no burst cluster reached the 8 dB threshold in any of the 1648 time bins. The pipeline still made 2 detections here (SNR peak 4.4 and 5.2 dB) and one candidate emitter — false alarms at the noise floor. |
| **101.420 – 101.490 MHz** | Flat noise: at most 2.3 dB of excess in any 10 kHz. The pipeline made 866 detections here (SNR peak up to 8.3 dB) across five candidate emitters, none confirmed. |
| 99.750 MHz ± 100 kHz | Nothing; the energy reaching this window is the 99.6999 MHz station's skirt. |
| 100.735 MHz ± 10 kHz, 101.050 ± 60 kHz, 101.160 ± 60 kHz, 101.700 ± 15 kHz | Flat noise (SNR −5.6 to −13.5 dB, 0/1648 bins present). |

## The earlier draft was wrong, and this is why it is worth writing down

The first version of this README carried a **draft** ground-truth list (G1–G7) taken
from the live inventory clusterer at capture time plus the user's ears. The analysis
pass measured every entry. It is superseded, and it was wrong in instructive ways:

- **G2, "steady carrier wave at 99.99 MHz"** — real energy, but a **receiver spur**,
  not an emission. Annotated as an artefact.
- **G3, "100.300 MHz narrowband data bursts, 1263 looks"** — the band was **silent**
  for the whole capture. The "looks" were the detector's own false alarms.
- **G6, "~101.45 MHz, dominant, strongest emission, 4810 looks"** — there is **no
  emission there**. The strong station is at **101.2987 MHz**, and 101.42–101.49 MHz
  is flat noise. The high look count was, again, a false-alarm cluster.
- **G1, "~99.75 MHz"** — the station is at **99.6999 MHz**; 99.75 MHz ± 100 kHz is empty.
- **G4, G5, G7** (100.735, 101.05–101.16, 101.70 MHz) — **all noise**, 0/1648 bins present.
- **100.4653 MHz** — a real, continuous, unidentified emission that **no draft entry
  mentioned at all**.

A "look count" from a live clusterer measures how often the *detector* fired, not how
strong a signal is; on this capture the two highest counts are both noise. That is the
property the acceptance test now guards.

## What the acceptance test asserts

`tests/e2e/tests/acceptance/fm_band_2026_09_15.rs`, through the **mock SDR device**,
with the truth stripped before the device sees the recording:

1. Every measured emission is **detected blind**, and the two measured to be WFM carry
   an FM-broadcast explanation in their top-k. The unidentified one asserts detection
   only — the band plan is never allowed to name what the measurement did not.
2. **Confirmed emitters are exactly the measured emissions** — each one reaches a
   Confirmed row, and no Confirmed row sits anywhere else (no ghosts).
3. **Measured silence stays silent**: no Confirmed emitter inside a measured-silent
   window, and every detection there is at least 5 dB below the weakest measured
   emission's strongest detection (measured margins: 9.3 dB and 6.2 dB).
4. **WFM + RDS from a second real capture**: the mode is chosen by the system, the
   pilot agrees with the analysis pass's discriminator measurement within two Welch
   bins, and the decoded PI matches the PI an independent reference decoder read from
   a different capture of the same station.
5. **Artefacts are flagged, not catalogued**: every detection on the reference harmonic
   and on DC carries the right `spur_reason`, and neither is confirmed as an emitter.

## Still open

- The 100.4653 MHz emission's modulation is unidentified. Identifying it needs either
  a longer look or a targeted demodulation pass.
- The reference-harmonic attribution is not retune-verified in this recording, because
  it is a single tune. A capture pair at two centres would settle it outright.
- The over-splitting and image/IMD behaviour the first draft hoped to exercise is
  visible here (the 101.42–101.49 MHz false-alarm cluster, five candidate emitters over
  noise), but as a *false-alarm* case, not as skirt fragments of a real station there.
