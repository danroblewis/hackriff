# Spike S5 — Blind estimation on real 8-bit HackRF captures

*(Written by the spike agent; saved to disk by the coordinator because the agent harness blocks subagents from writing report files.)*

**Date:** 2026-09-13 · **Spike:** S5 in [docs/09](../../docs/09-risks-and-spikes.md) (risk R4) · **Capabilities:** C13 param-estimation, C14 blind-symbol-estimation, C15 family (coarse) · **Gates:** T-010, T-011, T-013

Offline only, from the fixture store `fixtures/store/2026-09-13/`, no HackRF access. Throwaway research Python.

## Verdict: PASS (scoped); two parts INCONCLUSIVE

| Criterion | Result on real captures with independent truth | Status |
|---|---|---|
| **Symbol rate within ±1 %** | 915 MHz FHSS 2-FSK (100 / 150 kbit/s): **60 trusted, 0 wrong**, median error 1.1e-4, max 3.4e-4. RDS biphase: **100 % of windows ≥ 0.25 s within 1 %** (median error 5e-5 at 0.25 s, 3e-6 at 2 s). **0 trusted-and-wrong** anywhere (real, degraded-real, synthetic, RDS). | **PASS** |
| **Deviation within tolerance** | 915 FSK: **97 % within ±10 %** of the fixed-rate reference, median +1.5 % (p5 −2.6 %, p95 +6.5 %). Synthetic: 100 % within ±10 %. | **PASS** (vs a self-consistent reference — see caveat) |
| **Family correct above a stated SNR** | Real FSK: **100 % at ≥ 20 dB** in-band, 81 % at 15–20 dB; 6/414 labelled OOK, all at 6–15 dB. Synthetic: OOK/2-FSK/GFSK 100 % at ≥ 20 dB, BPSK/QPSK 100 % at ≥ 15 dB, 0 wrong labels in 900 runs. | **PASS for FSK**; OOK/PSK family on real signals **INCONCLUSIVE** (no real truth) |
| **Unknowns flagged** | 0 trusted rates and 0 labels on 200 noise snippets (915), 157 detector events on the empty 433 MHz file (noise, CW), synthetic NBFM voice, LoRa-like chirp, CW, noise (120). Real FM broadcast (analog): 0 trusted, 2/60 labelled FSK (untrusted). | **PASS** (2/60 analog mislabels noted) |

**What this means for the plan.** Blind estimation can be **authoritative when it reports `trusted`** — a trusted rate was never wrong. It abstains a lot on short weak bursts: 300–1500-symbol FSK needs **~15–20 dB in-band SNR** for trusted output, while the fixed-rate trial demodulator found preamble + sync down to ~0–6 dB. **Below the floor, priors + trial demodulation must lead** — the docs/09 "assistive" mode, applied only below the floor; not a FAIL. The inconclusive parts (OOK/PSK family on real signals, independent deviation truth) need the captures in §6.

## 1. Inputs and ground truth

| Capture | Settings (SigMF) | Truth source | Outcome |
|---|---|---|---|
| `ism_433p62M_2M_l24g30a1` | 180 s, 2 Msps, LNA 24 / VGA 30 / amp on | rtl_433 25.12 | **No device bursts.** rtl_433 decoded nothing (whole file at 2 Msps, plus 3 cut events at 250 kS/s). Noise rms 3.3 LSB. Detector found 157 events at +8–15 dB: noise plus one narrow CW line at 33.9 s near 433.6 MHz. **Negative controls only.** |
| `ism_915M_10M_l24g30a1` | 45 s, 10 Msps, LNA 24 / VGA 30 / amp on | see below | **685 bursts**, 1–16 ms, 50–360 kHz span, FHSS on a 200 kHz raster, up to +39 dB (`s5_ism915_spectrogram.png`). |
| `fm_100p8M_2p4M_l32g30a1` | 30 s, 2.4 Msps, LNA 32 / VGA 30 / amp on, station at +500 kHz | 19 kHz pilot | **RDS present.** CNR 19 dB in 200 kHz. Pilot 18 999.871 Hz → HackRF clock **−6.8 ppm**. |
| `adsb_1090M_*` | – | readsb | Skipped per coordinator (0 CRC-valid; antenna can't hear 1090 MHz). |

**915 MHz truth** (built, because known tools didn't apply): rtl_433 decoded 0/685 cut bursts at 1 Msps (`rtl433_truth.py`); an IEEE 802.15.4g SUN-FSK (Wi-SUN PHY) trial found no SFD with valid FCS (CRC-16/32, ± PN9, both bit orders; `sunfsk_truth.py`). Decoder-independent truth (`fsk_truth.py`): demodulate each burst at **fixed standard rates** R ∈ {50, 100, 150, 200, 300} kbit/s (channel filter, discriminator, best eye phase — none of the blind code); accept R only if ≥ 24 bits of 0101 preamble are immediately followed by sync `0000110001011111` (≤ 1 bit error) at exactly one rate (the sync is common to all strong bursts at both rates). **Result: 414 bursts** — 220 at 100 k, 193 at 150 k, 1 at 300 k (burst 234 at 2.9 dB; blind says 150 k untrusted → probably a false truth). Rate truth = R nominal (clock tolerances ≪ 1 %). **Deviation reference** = median |IF − centre| at symbol centres inside runs of ≥ 3 equal bits on the fixed-rate path → h ≈ 0.55 (27.5 kHz at 100 k, 42 kHz at 150 k). Caveat: self-consistent measurement, not a spec value (protocol unidentified, likely utility AMI mesh). Only PHY metadata kept (rate, preamble length, sync position, deviation); no payload decoded or stored.

**RDS truth:** RDS data rate = 57 000/48 = pilot/16 → in the capture's own clock **Rs = 1187.492 Bd**; biphase chip rate 2·Rs = **2374.984 Bd** is the physical symbol rate C14 must find (line-code resolution is C21's job).

## 2. Method

`s5lib.py` holds the prototype estimators (numpy/scipy); each returns a value, its evidence, and `null` with a reason when it declines.

1. **Detection** (`detect_bursts`): STFT, per-bin median floor, threshold (12 dB at 915, 8 dB at 433), dilation, connected components → time/frequency boxes.
2. **C13** (`c13_params`), snippet mixed to the box centre and decimated to ≥ 1.25 Msps: Welch PSD, noise-subtracted. **N0 = mean** Welch density of a signal-free pad (the median is ~1.6 dB low and gave SNR +9 dB on pure noise). OBW99 on the 5-bin-smoothed noise-subtracted PSD. SNR = unclipped in-band power / (N0·OBW), **re-measured over the burst extent**. CFO: centroid; x² line for DSB/BPSK.
3. **C14 cyclic lines** (`classify_and_estimate`), after recentring and a ±0.75·OBW channel filter: four raw lines in two independent groups — envelope (|x|², |d env|²) and phase (delay-multiply with D = fs/(2·OBW), |dIF|²) — each from a **locally whitened** periodogram (Blackman, 4× zero-pad, 24-bin block median); lines on a sloped continuum (PSK |x|²) are otherwise lost.
4. **Transition LS** (`rate_transitions_ls`) for OOK/FSK-like signals: seeded at each line candidate (f/4…2f) and a run-length unit (rtl_433 `-A` style); integer run counts; least squares with **separate rise/fall offsets per segment**; common factor k = 2–5 divided out. `ok` iff jitter < 0.12 UI, > 40 % odd run counts, ≥ 32 symbols, modal run count ≤ 90 % (a sliced tone/chirp is not a clock), **≥ 85 % of transitions kept**, and fit within 2 %.
5. **Consensus** (`rate_consensus`): harmonic-aware; an LS-confirmed candidate wins and its 2/3/4× lines count as support. **Trusted** iff lines ≥ 14 dB from ≥ 2 method groups, or LS ok + ≥ 1 line; transition-structured signals without LS need 3 lines; also requires digital structure (family score ≥ 0.5 or LS ok) and SNR_ext ≥ 0 dB. Harmonic alternatives (×½, ×2) always reported.
6. **Family scores:** OOK = two envelope levels, low level at noise, ≥ 12 level changes. FSK = IF at symbol centres for the top rate candidates is bimodal (Fisher J, histogram **valley** < 0.6, occupancy) and not periodic (tone/chirp). BPSK = single x² line + weak x line. QPSK = x⁴ line. Bare carrier → unknown. Label iff margin-weighted confidence ≥ 0.5 and SNR_ext ≥ 8 dB; else `unknown` with reasons.
7. **FSK deviation:** average IF over ±T/4 at LS symbol centres, keep only symbols whose neighbours share the decision (removes Gaussian ISI bias), median |v − mid|.

**Synthetic comparison** (`synth.py`, `eval915.py`, `synth_sweep.py`):
- **Matched twins** of the 40 strongest truth bursts: same R, deviation reference, preamble length and bit count; GFSK BT 0.5; embedded at a stated in-band SNR (C13 definition, measured OBW) in complex noise scaled to the real ADC noise (3.21 LSB rms); **8-bit quantised**; identical decimation → C13 → C14 chain.
- **Degraded real:** the same 40 bursts scaled down with noise added to keep 3.21 LSB rms, re-quantised.
- **Family sweep:** OOK 4 k, 2-FSK 9.6 k h = 1, GFSK 50 k h = 0.5, BPSK/QPSK 25 k α = 0.35; SNR 0–30 dB; 20 trials each.
- **RDS:** matched synthetic FM MPX (pilot, mono + stereo noise audio, biphase RDS with RDS cosine shaping), FM at 2.4 Msps, +500 kHz, real CNR, real ADC rms, 8-bit; RDS injection calibrated so subcarrier SNR matches the real 3.1 dB.

## 3. Results

### 3.1 915 MHz FHSS FSK — real bursts vs sync truth (bins = in-band SNR over burst extent)

| SNR bin (dB) | n | rate within 1 % | trusted | trusted & right | trusted & wrong | family = FSK | wrong label | deviation within 10 % (median error) |
|---|---|---|---|---|---|---|---|---|
| < 3 | 117 | 0.01 | 0.00 | 0 | 0 | 0.00 | 0 | – |
| 3–6 | 86 | 0.01 | 0.00 | 0 | 0 | 0.00 | 0 | – |
| 6–9 | 61 | 0.00 | 0.00 | 0 | 0 | 0.02 | 1 (OOK) | – |
| 9–12 | 37 | 0.03 | 0.00 | 0 | 0 | 0.19 | 2 (OOK) | – |
| 12–15 | 43 | 0.28 | 0.26 | 11 | **0** | 0.54 | 3 (OOK) | 0.80 (4.9 %) |
| 15–20 | 47 | 0.68 | 0.57 | 27 | **0** | 0.81 | 0 | 1.00 (2.0 %) |
| 20–25 | 20 | **1.00** | **1.00** | 20 | **0** | **1.00** | 0 | 1.00 (1.4 %) |
| ≥ 25 | 2 | 1.00 | 1.00 | 2 | 0 | 1.00 | 0 | 1.00 (1.0 %) |

- Blind h agrees with the fixed-rate reference: median 0.56 at 100 k, 0.58 at 150 k.
- 271 bursts without sync truth (mostly slivers, median −2 dB): 16 got a trusted rate, all within 1 % of 100 k or 150 k — plausibly real frames the truth search missed; no other rates claimed.
- Example strong bursts (blind vs truth): 330 → 100 012 (h 0.556); 113 → 100 015; 196 → 150 004; 247 → 150 041; 281 → 150 003; 101 → 100 015; LS jitter 0.04–0.06 UI.
- Compute (numpy, M3 Ultra, per 10–30 k-sample burst): C13 0.6–0.9 ms, C14 14–26 ms. Per event; a Rust port will be faster.

### 3.2 Synthetic vs real (same chain, same 8-bit quantisation)

| Set | At the strong bursts' own SNR (20–27 dB) | 15–20 dB | 12–15 dB | Trusted & wrong |
|---|---|---|---|---|
| Real bursts (all truth) | 22/22 right & trusted; deviation median error 1.0–1.4 % | 68 % right / 57 % trusted | 28 % / 26 % | 0 / 60 trusted |
| Matched synthetic twins | 22/22 right & trusted; deviation median error 2.2 % | 83 % / 83 % | 0 % (n = 4) | 0 / 19 |
| Real + added noise, re-quantised | – | 100 % / 60 % (n = 5) | 0 % (n = 6) | 0 / 3 |

Synthetic and real behave the same: same floor, same abstention pattern, no trusted errors. In 15–20 dB, synthetic twins trust slightly more often (83 % vs 57 %) — real bursts carry hop edges, multi-frame boxes and unknown shaping that add "multi_segment" gaps. C13 SNR bias on synthetic (measured − true): ≈ 0 dB at ≥ 20 dB, **−1 to −5 dB at 9–15 dB** (OBW99 shrinks as skirts vanish into noise), so the stated floors are conservative in true SNR. Figure: `s5_915_rate_vs_snr.png`.

### 3.3 Synthetic family sweep (20 trials per cell; trusted & wrong = 0 in all 900)

| Case | Family correct | Rate within 1 % | Trusted |
|---|---|---|---|
| OOK 4 kbit/s NRZ (200 sym) | 20 % at 15 dB, **100 % at ≥ 20 dB** | 50 % at 15 dB, **100 % at ≥ 20 dB** | 35 % at 15 dB, 100 % at ≥ 20 dB |
| 2-FSK 9.6 k, h = 1 (400 sym) | 75 % at 15 dB, **100 % at ≥ 20 dB** | 70 % at 15 dB, 100 % at ≥ 20 dB | same; deviation within 10 %: 100 % |
| GFSK 50 k, h = 0.5, BT 0.5 (500 sym) | 30 % at 15 dB, **100 % at ≥ 20 dB** | 20 % at 15 dB, 100 % at ≥ 20 dB | same; deviation within 10 %: 100 % |
| BPSK 25 k, α = 0.35 (500 sym) | 35 % at 12 dB, **100 % at ≥ 15 dB** | 65 % at 12 dB, 100 % at ≥ 15 dB | 90 % at 15 dB, 100 % at ≥ 20 dB |
| QPSK 25 k, α = 0.35 (500 sym) | 40 % at 12 dB, **100 % at ≥ 15 dB** | 80 % at 12 dB, 100 % at ≥ 15 dB | 80 % at 15 dB, 100 % at ≥ 20 dB |

Below the family floor the chain says `unknown` (low_snr, ambiguous_family), never a wrong label. Figure: `s5_synth_family_rate.png`.

### 3.4 RDS (real FM station; subcarrier in-band SNR 3.1 dB)

C13 on the subcarrier (deliberately mixed at a wrong 56.7 kHz): CFO via x² line +299.61 Hz vs +299.61 Hz true (pilot × 3); centroid gave +235.5 Hz; OBW99 4.0 kHz.

C14 vs chip rate 2·Rs:

| Window | Real: within 1 % / trusted / trusted & wrong / median error | Synthetic: within 1 % / trusted / trusted & wrong / median error |
|---|---|---|
| 0.10 s | 0.95 / 0.82 / 0 / 2.3e-4 | 0.97 / 0.97 / 0 / 1.9e-4 |
| 0.25 s | 1.00 / 1.00 / 0 / 4.9e-5 | 1.00 / 1.00 / 0 / 6.0e-5 |
| 0.5 s | 1.00 / 1.00 / 0 / 1.8e-5 | 1.00 / 1.00 / 0 / 1.9e-5 |
| 1 s | 1.00 / 1.00 / 0 / 6.8e-6 | 1.00 / 1.00 / 0 / 3.8e-6 |
| 2–10 s | 1.00 / 1.00 / 0 / ≤ 2.8e-6 | 1.00 / 1.00 / 0 / ≤ 2.8e-6 |

- In every window three lines (|x|², delay-multiply, |dIF|²) agree at 2375.0 Hz; the **data rate 1187.5 is offered as the ½ harmonic alternative in 100 % of windows**.
- Family stays `unknown` (3 dB < 8 dB family floor) — correct abstention.
- Noise added to the real subcarrier (within 1 % / trusted): +2, 0 dB → 1.00 / 1.00 (0.5 s and 2 s); −3 dB → 1.00 / 0 (below the 0 dB rate floor); −6 dB → 0.50 (0.5 s), 1.00 (2 s); −9 dB → 0.15, 0.64; −12 dB → 0.00. Synthetic matches (−6 dB 0.75 / 1.00; −9 dB 0.00 / 0.89). Trusted & wrong = 0 everywhere. Figure: `s5_rds.png`.

### 3.5 Negatives (must be unknown and untrusted)

| Set | n | Labelled | Trusted rate |
|---|---|---|---|
| 915 noise snippets (no detections nearby) | 200 | 0 | 0 |
| 433 MHz detector events (noise, CW) | 157 | 0 | 0 |
| Real FM broadcast, 20 ms windows (analog) | 60 | 2 (FSK, at 14 dB) | 0 |
| Synthetic NBFM voice, 10/20/30 dB | 30 | 0 | 0 |
| Synthetic LoRa-like chirp BW 125 k SF7, 10/20/30 dB | 30 | 0 | 0 |
| Synthetic CW, 10/20/30 dB | 30 | 0 | 0 |
| Synthetic noise only | 30 | 0 | 0 |

## 4. Pitfalls T-011 must not repeat

Each produced confident wrong answers in an earlier iteration of this spike:
1. **|x_c²| is |x|².** A "conjugate-cyclic" rate line computed as `abs(x_c**2)` duplicates the envelope line; two "agreeing" methods made BPSK/QPSK/chirp/FM-pilot rates trusted and wrong. Count agreement only across independent groups (envelope vs phase). Use x² for CFO and family only.
2. **Global-max line pickers fail on PSK:** the |x|² line at Rs sits on a sloped continuum — whiten locally before picking.
3. **Median-based N0 inflates SNR** on noise (+9 dB on pure noise) — use the mean.
4. **NRZ OOK has no |x|² line at Rs** (sinc² null) — use the envelope derivative and run-length seeds.
5. **Outlier rejection hides a wrong clock:** a 2.5× seed on OOK "fitted" at 0.035 UI by discarding half the transitions. Require ≥ 85 % kept transitions, separate rise/fall offsets, a common-factor check and the odd-run fraction.
6. **Harmonics are the dominant failure:** a 0101 preamble gives a line at Rs/2, biphase gives 2Rs, sharp OOK edges give 3–5× lines. LS-confirmed candidates must outrank line votes; always emit harmonic alternatives.
7. **GFSK h = 0.5 looks like BPSK** in x² coherence (MSK-like signals have two x² lines) — test that the x² line is unique.
8. **Chirps and FM pilots fool IF bimodality and LS** (periodic decisions/run counts) — test for periodicity; data must carry information.
9. **Multi-frame detector boxes** (hop edges, ack gaps) mask lines — analyse the longest on-segment.
10. **Rate trust ≠ family trust:** lines integrate over time (RDS rate fine at 0 dB) but family features need 15–20 dB — gate them separately.

## 5. Recommendations

### T-010 (C13, `hk-estimate`)
- N0 from C08 or a signal-free pad as the **mean** Welch density. OBW99 from the 5-bin-smoothed noise-subtracted PSD. SNR = S/(N0·OBW99), **unclipped**, **re-measured over the burst extent** (first to last sample above N0 + 6 dB after a ±0.75·OBW filter).
- CFO: spectral centroid by default; **x² line** for DSB/BPSK (exact on RDS, centroid 64 Hz off); FSK: mid-point of symbol-centre clusters.
- Report the extent SNR and the SNR bias caveat (−1…−5 dB at 9–15 dB true SNR).
- Where a pilot exists (19 kHz), feed the clock ppm to C05 (the spike measured −6.8 ppm).

### T-011 (C14 blind rate + deviation, `hk-estimate`)
Port the `s5lib.classify_and_estimate` structure:
- Channel filter ±0.75·OBW99; IF smoothing fs/(2·OBW); delay D = round(fs/(2·OBW)).
- Lines: |x|², |d env|², delay-multiply, |dIF|² from a whitened periodogram (Blackman, 4× zero-pad, 24-native-bin block median). A line counts at **≥ 12 dB**; line-only trust needs **≥ 14 dB from both groups**. Search from max(4·fs/N, OBW/50) to min(1.2·OBW, fs/2.5).
- Candidates: {f/4, f/3, f/2, f, 2f} of each line + a run-length seed.
- Transition LS: separate rise/fall offsets per segment (split at gaps > 64 T); common factor k ∈ 2..5; `ok` iff jitter < 0.12 UI, odd runs > 40 %, ≥ 32 symbols, modal run ≤ 90 %, kept ≥ 85 %, fit within 2 % of the seed.
- Trust: LS ok + ≥ 1 line (direct or harmonic), or ≥ 2 groups ≥ 14 dB; FSK/OOK-like without LS needs 3 direct lines; plus digital structure and SNR_ext ≥ 0 dB. Emit top-k with ×½ and ×2 alternatives.
- Family gates: label only at SNR_ext ≥ 8 dB and confidence ≥ 0.5; measured reliable floors **PSK ≥ 15 dB, FSK/OOK ≥ 20 dB** for 200–1500-symbol bursts. FSK bimodality at symbol centres: J ≥ 2.5 (score saturates at 4.5), valley < 0.6, occupancy > 10 %, periodicity < 0.95. BPSK: second x² line < 0.6 of the first. OOK: contrast ≥ 8 dB, low level < 2.5 σ_noise.
- Deviation: median |IF − mid| at LS symbol centres (±T/4 average) with same-decision neighbours.
- Acceptance fixtures: the 915 bursts + sync truth list (`fsk_truth.py`); the RDS window set (truth = pilot/16); the negatives list. Keep `smoke.py`-style regressions that fail on any trusted-and-wrong.
- Real path: delay-mult and IF-diff lines are cheap; SSCA not needed for these signals.

### T-013 (FSK demod + framing/CRC inference)
- The spike already demonstrates framing inference: fixed-rate trial demod at standard rates + cross-burst preamble/sync search found a **common 16-bit sync in 414/685 bursts** where rtl_433 and the 802.15.4g SFD/CRC trial failed. Next: PHR/length/CRC inference on those frames (no CRC identified in this spike).
- Weak bursts (< 15 dB in-band) should be decoded by **prior-led trial demodulation**, not blind C14, seeded from (a) trusted C14 results on strong bursts of the same emitter cluster (rate, deviation, channel raster) and (b) a standard-rate table. The fixed-rate path found sync at extent SNR 0–6 dB.

### SNR floor for trustworthy estimates (in-band SNR = S/(N0·OBW99) over the burst extent)

| Signal type | Trusted and correct | Partial | Abstains (never wrong) below |
|---|---|---|---|
| Short bursts, 200–1500 symbols (FSK/OOK) | **≥ 20 dB** | 15–20 dB (57–83 % trusted) | 12–15 dB |
| Short linear PSK (500 symbols) | **≥ 15 dB** | – | – |
| Long/continuous (RDS, ≥ 0.25 s ≈ 600 chips) | **≥ 0 dB** (rate) | – | accurate to −3 dB at 0.5 s and −6 dB at 2 s, deliberately untrusted below 0 dB |
| Family labels | FSK/OOK ≥ 20 dB, PSK ≥ 15 dB | – | – |

## 6. What would settle the inconclusive parts
1. **Real OOK/ASK family + rate truth:** a 315/433.92 MHz sensor or remote **the user owns**, decodable by rtl_433, captured at ≥ 20 dB in-band (place it near the antenna; the 180 s 433 MHz capture had no devices).
2. **Real PSK / 4-level truth:** a metadata-only capture of AIS (GMSK 9600, CRC truth), NOAA/Meteor LRPT QPSK, or a P25/DMR repeater.
3. **Independent deviation truth:** a device with documented deviation (an rtl_433-supported FSK sensor with a published spec, or the user's own 802.15.4g/Wi-SUN dev kit).
4. **Weak-burst ROC:** a 902–928 MHz capture with a better antenna or preselector, to populate the 6–15 dB bins with truth.

## 7. Reproduce

Data store is read-only; converted/cut files go to scratch only.

```sh
cd spikes/s5-blind-estimation
ST=/Users/daniellewis/hackriff/fixtures/store/2026-09-13
S=/path/to/scratch   # outputs (bursts, truth, results JSON)

python3 specsurvey.py $ST/ism_915M_10M_l24g30a1.sigmf-data 10e6 915e6 $S/ism915 8192 4 10   # survey + spectrogram
python3 run_detect.py $ST/ism_915M_10M_l24g30a1.sigmf-data   $S/b915.json 1024 4 12 3
python3 run_detect.py $ST/ism_433p62M_2M_l24g30a1.sigmf-data $S/b433.json 512 4 8 3
python3 rtl433_truth.py $S/b433.json $S/cut433 $S/rtl433_433.json 250000 0.2 50 51 52   # 0 decodes
python3 rtl433_truth.py $S/b915.json $S/cut915 $S/rtl433_915.json 1000000 0.02          # 0 decodes
python3 sunfsk_truth.py $S/b915.json $S/sun.json 12                                     # 0 CRC-valid
python3 fsk_truth.py    $S/b915.json $S/truth915.json 0                                 # 414 sync truths
python3 eval915.py $S $S 12          # §3.1, §3.2, §3.5
python3 synth_sweep.py $S 20 8       # §3.3
python3 rds.py $ST/fm_100p8M_2p4M_l32g30a1.sigmf-data $S 30   # §3.4
python3 smoke.py                     # regression: prints "problems: 0"
python3 make_plots.py $S .
python3 inspect_bursts.py $S/b915.json $S 330 113 196          # per-burst diagnostics + PNG
```

- Dependencies: numpy 2.3, scipy 1.16, matplotlib 3.10 (BSD), research only; rtl_433 (GPL) used only as an external truth tool, not linked. Nothing here enters the product or the ADR-0010 ledger.
- Files: `s5lib.py` (detection, C13, C14); `synth.py` (generators, 8-bit embedding); truth scripts `fsk_truth.py`, `sunfsk_truth.py`, `rtl433_truth.py`; evaluation `eval915.py`, `synth_sweep.py`, `rds.py`, `smoke.py`; surveys `survey.py`, `specsurvey.py`, `run_detect.py`; `inspect_bursts.py`, `make_plots.py`, 4 PNGs.
