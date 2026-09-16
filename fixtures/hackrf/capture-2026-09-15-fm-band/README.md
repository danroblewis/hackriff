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
| **100.465339 MHz** | 28.1 kHz | 7.02 dB | 1646/1648 bins | **Harmonic 43 of a free-running ~2.3364 MHz oscillator** — a local unintentional emitter, identified by T‑317 (see below). Anticipated by nobody. Continuous and very steady. No 19 kHz pilot (+0.2 dB), no RDS, no carrier line, no symbol structure: it carries no information. Its IQ mirror at 101.1347 MHz is empty, so not a receiver image. |

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
- **100.4653 MHz** — a real, continuous emission that **no draft entry mentioned at
  all**, and that took a second pass (T‑317) to name.

A "look count" from a live clusterer measures how often the *detector* fired, not how
strong a signal is; on this capture the two highest counts are both noise. That is the
property the acceptance test now guards.

## Identifying the 100.4653 MHz emission (T‑317)

T‑289 could only record what it is **not**. One 2.4 MHz window was never going to be
enough, because the evidence is outside it. The three 2026‑09‑13 captures of the same
site include two at a **98 MHz centre and 20 Msps**, spanning 90.6–105.4 MHz, and they
show the emission has **siblings**:

| capture | n = 43 | n = 44 | n = 45 | f₀ = fₙ/n |
|---|---|---|---|---|
| 98 MHz / 20 Msps, **amp off** | 100.446474 | 102.782818 | 105.118406 MHz | 2.335965 / 2.335973 / 2.335965 MHz |
| 98 MHz / 20 Msps, **amp on** | 100.422778 | 102.757154 | 105.093087 MHz | 2.335413 / 2.335390 / 2.335402 MHz |

Three independent estimates of the fundamental agreeing to **9 Hz (3.7 ppm)**, and
fitting `f = n·f₀ + b` gives **b = +67 Hz** against an f₀ of 2.336 MHz — an index off by
one would put b at ±2.34 MHz, so the harmonic numbers are pinned exactly.

Two things make this an identification rather than a coincidence:

- **The width scales with the harmonic number.** rms spectral width ÷ n is 138, 120 and
  137 Hz (amp off) and 136, 134 and 130 Hz (amp on). The fundamental carries ~134 Hz rms
  of frequency noise; harmonic 43 carries 43× it, 5.8 kHz. Three unrelated emitters
  cannot do that. Their normalised line shapes also correlate at 0.967–0.988.
- **It moves.** The n = 43 member measured 100.422778, 100.443522, 100.446474 and
  100.464646 MHz across the four captures — 41.9 kHz of spread, f₀ drifting 417 ppm over
  two days. The two broadcast stations in this capture are within 1.34 kHz and 56 Hz of
  their channels.

And it carries nothing. The modulus is constant (|z| kurtosis 1.386 against 2.03 for
noise-only controls), there is no carrier line at 1.14 Hz RBW, x² and x⁴ recover no
carrier or symbol rate (3.2 and 4.8 dB peaks), the instantaneous-frequency histogram is
unimodal, and the discriminator's 0.1–1 kHz power is constant to **0.30 dB** standard
deviation across 45 one-second blocks. Speech and music vary second to second; this does
not. The 28 kHz is the fundamental's frequency noise, multiplied by 43.

The band plan offers exactly one row here — `fm-broadcast`, 88–108 MHz (47 CFR 2.106) —
and the measurement contradicts it on every count: 28 kHz against 120–128 kHz for the
real stations in the same survey, 34.7 kHz off the nearest US channel, no pilot, no RDS,
no programme, and a centre that wanders tens of kHz between sessions. Ranked the way the
product vision asks: *looks like a multiplied free-running oscillator; FM broadcast is
the only thing allocated here; 34.7 kHz off raster and 100 kHz too narrow to be one.*
The annotation's `identification` block carries the evidence and the ten hypotheses it
excludes; it stays `role = emission`, not `artefact`, because it is energy the receiver
took in, not a spur the tuner synthesised.

## A capture-chain artefact this fixture carries (T‑317)

The raw ci8 stream has a **periodic gain step**: samples 0–895 of every 8192-sample
period are **0.431 dB** lower in power than samples 896–8191, flat either side of the
step, measured over all 13,184 whole periods. It is stream-wide — it shows in the
wideband total power, on the receiver's own DC line (27 dB) and reference harmonic
(16 dB), and in bands with **no emission at all** (23 dB at 100.150 MHz).

It puts a comb at **292.969 Hz (= 2.4 MHz / 8192)** and at least 27 harmonics into the
amplitude and, through a channel filter, the discriminator of anything narrowband and
weak, with the sinc nulls of a 896/8192 duty cycle near harmonics 9 and 18. Taken at
face value it reads as a 3.41 ms TDMA frame that is not there, and it did exactly that
during this investigation. **Anything looking for periodicity, frame rates or
cyclostationarity in this fixture must exclude n × 292.969 Hz.** The source is not
identified; the 2026-09-13 2.4 Msps capture of the same device carries it too. Recorded
in `hackriff:provenance.capture_artefact`.

## What the acceptance test asserts

`tests/e2e/tests/acceptance/fm_band_2026_09_15.rs`, through the **mock SDR device**,
with the truth stripped before the device sees the recording:

1. Every measured emission is **detected blind**, and the two measured to be WFM carry
   an FM-broadcast explanation in their top-k. The oscillator harmonic asserts detection
   only — the band plan is never allowed to name what the measurement did not, and the
   only row it has here (`fm-broadcast`) is the one the measurement rules out.
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

- The 100.4653 MHz oscillator harmonic is identified as a family, but **the emitter is
  not**: nothing here says which device runs at 2.336 MHz, or whether it radiates into
  the antenna or couples into the receive chain conducted. All four captures share one
  antenna and host, and the 20 Msps pair is quantisation-limited, so turning the amp off
  moved the family's excess over the floor no more than it moved the real stations'. A
  capture with a **50 Ω terminator** in place of the antenna would settle it.
- The 8192-sample gain step above has no identified source.
- Nothing in the system spots a harmonic family. Image and reference-harmonic
  attribution exist (T-302); "these three emitters are harmonics of one fundamental
  nobody can see" is a capability that does not.
- The reference-harmonic attribution is not retune-verified in this recording, because
  it is a single tune. A capture pair at two centres would settle it outright.
- The over-splitting and image/IMD behaviour the first draft hoped to exercise is
  visible here (the 101.42–101.49 MHz false-alarm cluster, five candidate emitters over
  noise), but as a *false-alarm* case, not as skirt fragments of a real station there.
