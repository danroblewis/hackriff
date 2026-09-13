# Spike S4 — Detection quality in urban overload

*(Written by the spike agent; saved to disk by the coordinator because the agent harness blocks subagents from writing report files.)*

**Date:** 2026-09-13 · **Risk:** R3 (docs/09) · **Feeds:** T-005 (noise floor), T-006 (CFAR + Detection records), C05/C08/C09, ADR-0005
**Mode:** offline analysis of captures already in the fixture store (no hardware access, receive-only captures, detection/metadata only — nothing was demodulated).

## Verdict: INCONCLUSIVE

The chain behaved as the hypothesis requires wherever it could be tested. The capture set could not test the two things the pass criterion actually hinges on.

| Sub-criterion (docs/09 §S4) | Result | Status |
|---|---|---|
| Known emitters still detected | 7/7 strong FM stations detected and **none suppressed** at mid gain, at the clipped high gain and after the retune. 0 of 13 raster stations falsely flagged by any ghost test. At the lowest gain, 0/7 at emitter level (quantisation-limited). | PASS |
| Spurs/LO artefacts flagged | 100.000 MHz reference harmonic and DC flagged in every capture; the retune test correctly moved DC with the LO; a 14-line, 209.48 kHz narrowband comb flagged at high gain (chance probability 0/200). | PASS |
| IMD/image ghosts flagged or suppressed | **No real gain-dependent ghosts existed**: 0 of 38 high-gain emitters failed the gain-step test; no images found. On a semi-synthetic positive control (strong carriers + cubic nonlinearity, 10 dB drive step) both predicted IM3 ghosts were flagged; all 8 flagged detections were genuine 3rd-order products; 0 of 11 real stations flagged. | Mechanism verified semi-synthetically; untested on real ghosts |
| False-alarm rate < 1 false emitter/MHz/h after masking | Ideal noise, recommended chain: **0 false boxes in 3.31 MHz·h (95 % upper bound 0.91 /MHz/h)**. Real quiet spans: 4 s × 5.4 MHz = 0.006 MHz·h exposure, so one box = 166 /MHz/h. Best real config: 0–17 boxes per capture. Per-cell exceedance tails 10–110× design in real frames. Persistent unexplained emitters after masking ≤ 0.31 /MHz (upper bound, counted once). | Not establishable from 4 s captures; box-level real rate very likely above target |
| Compare with FM notch in line | Not captured | Not done |

**Why not PASS:** weak-signal, indoors-ish scene with an unknown antenna. Even LNA 32 / VGA 30 / amp on (+33 dB measured over mid) produced ADC clipping (1.17 % of samples) but no measurable intermod — not the "busy urban overload" the hypothesis is about. Real false-alarm rates cannot be bounded below ~500 /MHz/h (zero-count 95 % bound) with 4 s of data.

**Why not FAIL:** the FAIL condition (inventory filling with ghosts) did not occur; every observed artefact class was flagged by at least one rule; known emitters survived all masks.

## 1. Inputs

Store `fixtures/store/2026-09-13/` (read-only). ci8 at 20 Msps, 4 s each; HackRF One (board < r6, fw 2026.01.3), internal clock, unknown antenna, indoors-ish.

| Capture | Tune | LNA/VGA/amp | Mean power (hackrf_transfer) | Rail std | Clipped samples | FCME floor (median, dBFS/Hz) |
|---|---|---|---|---|---|---|
| urban_98M_20M_l8g10a0 | 98 MHz | 8/10/off | −35.2 dBFS (AC −47.3) | 0.50 code | 0 | −120.3 (quantisation-limited) |
| urban_98M_20M_l24g20a0 | 98 MHz | 24/20/off | −32.9 dBFS (AC −37.7) | 1.17 code | 0 | −115.1 |
| urban_98M_20M_l32g30a1 | 98 MHz | 32/30/on | −6.3 dBFS | 43.7 code | 956 151 (1.17 %, every frame) | −84.6 |
| urban_99M_20M_l24g20a0 | 99 MHz | 24/20/off | −32.8 dBFS | 1.17 code | 0 | −115.0 |
| urban_915M_20M_l8g10a0 | 915 MHz | 8/10/off | −35.3 dBFS | 0.49 code | 0 | −120.5 (quantisation-limited) |
| urban_915M_20M_l32g30a1 | 915 MHz | 32/30/on | −22.4 dBFS | 6.7 code | 0 | −97.2 |

- DC offset ≈ (+0.6, −2.3) codes in every capture; at low gain it dominates the mean power.
- At LNA 8/VGA 10 the floor is −120.3 dBFS/Hz at both 98 and 915 MHz — the ADC quantisation floor, not RF noise. The mid-gain floor sits only +5.2 dB above it.
- Sweeps: `hackrf_sweep` 1–1000 MHz at 24/20/off and 32/40/on (20 sweeps, 98 kHz bins); 1–6 GHz at 32/30/on (10 sweeps, 455 kHz bins). Each sweep row is a single FFT (measured n_eff ≈ 1.2), so the 20-sweep mean is treated as Gamma(20).

## 2. Method

Code in this directory (research Python only; the product path is Rust `hk-dsp`/`hk-detect`).

1. **PSD/STFT** (`s4lib.stft_power`): Hann, nfft 4096 (RBW bin 4.88 kHz), 10 non-overlapping FFTs averaged per frame (2.05 ms) → frame values Gamma(10). Welch PSD = mean over 4 s. Per-frame clip fraction and spectral kurtosis kept.
2. **Noise floor** (`s4lib.fcme`, `percentile_floor`, `minstat_floor`, `blockwise`): FCME on 256-bin blocks (hop 64), T_CME from Pfa 1e-3 for Gamma(n), init 10 %, truncated-mean bias corrected, interpolated in dB, computed per frame (static floor = time median). Block percentile p50/p20, bias-corrected for Gamma(n). Minimum statistics per bin: IIR α 0.7, 256-frame (0.5 s) sliding minimum, Monte-Carlo bias correction.
3. **Frame-level detector** (`s4lib.detect`): OS-CFAR across frequency (N 32, G 4, k 24; α computed numerically for Gamma(n) order statistics) ORed with a floor-referenced branch (P > T·floor), 3 dB floor guard on the OS branch. Hysteresis: seeds at Pfa_on, region at Pfa_off; 4-connected components of the raw region containing a seed and lasting ≥ min_frames survive; survivors then merged across ≤ 2-frame gaps.
4. **Emitter-level detector** (`s4lib.detect_integrated`): 4 s spectrum vs FCME floor, seed +6 dB, extend +3 dB (with n_eff ~ 2·10⁴ the statistical threshold is ~0.2 dB, so the model-uncertainty guard dominates).
5. **In-capture flags** (`analyze_iq.in_capture_flags`, `comb_flags`): `spur_candidate` = narrow (≤ 25 kHz) within max(10 kHz, 25 ppm·f) of n × 10 MHz; `dc` = ≤ 40 kHz wide within 15 kHz of fc; `edge` = |f − fc| > 8 MHz (excluded from metrics; baseband filter 15 MHz); `image_candidate` = ≥ 20 dB stronger emitter at mirror 2fc − f with mirrored-shape correlation > 0.5; `clipped` = clip fraction > 1e-4 in the frame; `marginal` = peak SNR < 10 dB; `comb` = ≥ 6 narrow lines on an arithmetic grid (tol 1.5 kHz) with Monte-Carlo chance < 5 %; `impulsive` frame = band-median P/floor > 0.5 dB above its time median.
6. **Gain-step test** (`analyze_iq.gain_step`, SNR invariance): with an analog-noise-limited floor a real signal keeps its SNR across a gain step while IMᵢ gains (i − 1) dB of SNR per dB. Allowed real-signal ΔSNR `bound = max(0, G_lin − Δfloor)`, G_lin = median level change of ≥ 3 strong, wide, non-spur anchors (measured, not nominal). `suspect_imd` if ΔSNR > bound + 6 dB (lower bound used if the lower-gain SNR is below the measurability limit, 0.5 dB excess ≈ −9.1 dB SNR); `compressed` if level grew < G_lin − 6 dB; `inconclusive_bursty` if the 0.5 s block SNR spread > 6 dB.
7. **Retune test** (`analyze_iq.retune`): 98 → 99 MHz, same gain, common non-edge span 91–106 MHz. Labels: stays / moves ±Δ (`lo_relative`) / moves 2Δ (image) / not reproduced.
8. **IM3 prediction:** 2fa − fb products of the 6 strongest clean emitters, with chance-coincidence fraction.
9. **Quiet spans (false alarms):** FM captures — 88–108 MHz is allocated only to continuous FM broadcast, so a frame-level detection in a span with no continuous emission is a false alarm (or at least not an emitter). A span is quiet where the 4 s spectrum is < 1 dB above floor in both 98 and 99 MHz mid captures, ≥ 50 kHz from any emitter in any FM capture, ≥ 25 kHz from DC and n × 10 MHz, and ≥ 20 bins wide; the same absolute spans (5.42 MHz) are applied to all four FM captures. 915 MHz: same rule using the high-gain capture (13.05 MHz) — an **upper bound**, since real ISM bursts occur there.
10. **Semi-synthetic IMD control** (`imd_control.py`): two FM-like carriers at −20 dBFS (95.1, 97.7 MHz — odd tenths, so IM3 lands on the raster at 92.5/100.3 like a real ghost) added to the real 98 MHz mid IQ, then y = gx(1 − a|gx|²) at drives 0 and +10 dB, noise scaled with drive, no 8-bit requantisation; a = 0 is the negative control.
11. **Sweeps** (`sweep_survey.py`): same FCME + OS-CFAR (N 16, G 2, k 12, n 20) on sweep-mean spectra; gain-step test between the two 1–1000 MHz sweeps; IM2/IM3 prediction from the 10 strongest low-gain detections.

## 3. Results

### 3.1 Noise-floor estimators

Synthetic bias (dB, median over bins; 4096 bins; signals 5–200 bins wide at 3–30 dB SNR; `results/synth_check.json`):

| Occupancy | FCME (block 256) | p50 (block) | p20 (block) | min-stat per bin | | bursty 30 % duty: FCME | p50 | p20 | min-stat |
|---|---|---|---|---|---|---|---|---|---|
| 0 % | 0.00 | 0.00 | 0.01 | +0.17 | | 0.00 | 0.00 | 0.01 | +0.17 |
| 23 % | 0.00 | +0.30 | +0.18 | +0.27 | | 0.00 | +0.01 | +0.01 | +0.25 |
| 41 % | 0.00 | +2.05 | +0.71 | +0.43 | | 0.00 | +0.07 | +0.09 | +0.41 |
| 61 % | 0.00 | +9.97 | +1.54 | +9.12 | | 0.00 | +0.28 | +0.18 | +1.26 |
| 80 % | 0.00 | +15.1 | +6.05 | +14.6 | | 0.00 | +0.37 | +0.24 | +2.25 |

Real captures — estimator minus FCME (median over non-edge bins) and on emitter bins (emitters are 11–15 % of FM non-edge bins):

| Capture | p50 | p20 | min-stat (all) | min-stat on emitter bins | FCME − quiet-span PSD* |
|---|---|---|---|---|---|
| 98M low | −0.01 | −0.02 | −0.07 | **+14.0** | +0.12 |
| 98M mid | +0.14 | −0.06 | −0.13 | **+5.3** | +0.25 |
| 98M high (clipped) | +0.22 | −0.09 | −0.17 | **+5.6** | +0.44 |
| 99M mid | +0.16 | −0.05 | −0.15 | **+4.2** | +0.35 |
| 915M high | 0.00 | +0.01 | +0.08 | +0.29 | −0.09 |

\*Quiet spans were selected as < 1 dB above floor, so this column is biased positive by construction. Treat real-data floor uncertainty as ±0.5 dB.

**Takeaway:** FCME is unbiased to 80 % occupancy. Percentiles fail at ≥ 40 % (p50) / ≥ 60 % (p20). Per-bin min-stat reads continuous carriers as floor (+4 to +14 dB on emitter bins), so it must not be the CFAR reference. On these lightly occupied real spectra all block estimators agree within 0.3 dB.

### 3.2 CFAR threshold calibration

- α for Gamma(10), N 32, k 24: Pfa 1e-2 → 2.14 dB, 1e-3 → 3.00, 1e-4 → 3.67, 1e-5 → 4.22, 1e-6 → 4.70 dB. Numeric α matches the closed form for n = 1 (14.3985 vs 14.3985); Monte Carlo on ideal Gamma noise hits design Pfa within 4–14 % down to 1e-5.
- Floor-branch T for Gamma(10): 1e-3 → 3.55 dB, 1e-6 → 5.15 dB.
- Synthetic 8-bit noise (quantised Gaussian at 0.35/0.5/1.2 codes per rail with the measured DC offset, same STFT): 1.15–1.5× design Pfa. The excess comes from Hann-correlated reference bins, not quantisation. **8-bit quantisation alone does not break CFAR calibration.**
- Real quiet spans (`figs/fig3_cfar_calibration.png`), measured per-cell exceedance ÷ design:

| Capture | OS @1e-4 | OS @1e-6 | Floor @1e-4 | Floor @1e-6 | Impulsive frames |
|---|---|---|---|---|---|
| 98M low | 1.1× | < 0.3× (0 in 310k) | 1.2× | 4× | 0 % |
| 98M mid | 4.9× | 10× | 61× (1.8× without impulsive frames) | 4400× (8× without) | 4.4 % |
| 98M high (clipped) | 15× | **110×** | 7.7× | 120× | 1.6 % |
| 99M mid | 4.0× | 13× | 52× | 3700× | 3.2 % |
| 915M high | 3.3× | 34× | 17× | 790× | 11.5 % |

Low gain matches theory. At mid gain a **static** floor is wrecked by broadband impulsive frames (`figs/fig4_impulsive_frames.png`): the top 1 % of frames carry 38–47 % of all exceedances, up to 417 of 1109 quiet bins in one 2 ms frame. They also appear in the unclipped 99 MHz capture, so they are environmental man-made noise, not overload. The OS branch adapts within the frame and is far more robust. Clipping fattens the tail: at the clipped gain the OS tail at 1e-6 is 110× design — the largest overload effect on detection statistics measured.

### 3.3 Frame-level false alarms

Ideal Gamma noise with a known floor (`results/synth_chain_fa*.json`; 4096 bins, 2.05 ms frames):

| Config (on / off Pfa, min frames) | Exposure | False boxes | Rate (/MHz/h) | 95 % upper bound |
|---|---|---|---|---|
| **OR, 1e-6 / 1e-3, min 3 (recommended)** | 3.31 MHz·h | 0 | 0 | **0.91** |
| OS only, 1e-6 / 1e-3, min 3 | 3.31 MHz·h | 0 | 0 | 0.91 |
| OR, 1e-6 / 1e-3, min 2 | 1.32 MHz·h | 1 | 0.76 | 3.6 |
| OR, 1e-7 / 1e-3, min 2 | 1.32 MHz·h | 0 | 0 | 2.3 |
| OR, 1e-4 / 1e-3, min 3 | 1.32 MHz·h | 0 | 0 | 2.3 |
| OR, 1e-6, fixed −3 dB hysteresis, min 2 (docs/04-style) | 1.32 MHz·h | 47 | 35.5 | 45 |

**Two design bugs found and fixed:** a fixed −3 dB hysteresis at n = 10 puts the off level only ~2 dB above mean noise, so noise percolates and false-box counts become *non-monotonic* in Pfa; and gap-closing before the duration test lets noise satisfy min-duration. Fix: derive the off threshold from its own Pfa, and test duration on raw components before merging.

Real quiet spans (`results/metrics.json → false_alarm`): raw boxes / boxes outside impulsive frames / false "tracks" (≥ 2 boxes within ±1 bin). FM exposure 0.0060 MHz·h (1 box = 166 /MHz/h); 915 exposure 0.0145 MHz·h (1 box = 69 /MHz/h).

| Config | 98M low | 98M mid | 98M high (clipped) | 99M mid | 915M low | 915M high† |
|---|---|---|---|---|---|---|
| OR, static 4 s floor, 1e-6 / 1e-3 / min 3 | 2/2/1 | 37/13/3 | 14/14/3 | 28/19/3 | 1/1/0 | 21/21/2 |
| **OR, per-frame FCME floor, 1e-6 / 1e-3 / min 3** | **1/1/0** | **0/0/0** | **13/13/3** | **9/9/3** | **1/1/0** | **17/17/1** |
| OS only, 1e-6 / 1e-3 / min 3 | 1/1/0 | 0/0/0 | 10/10/3 | 11/11/3 | 0/0/0 | 1/1/0 |
| Floor only (static) | 0/0/0 | 33/12/3 | 7/7/1 | 18/11/1 | 1/1/0 | 19/19/2 |
| OR, 1e-6 / 1e-3 / min 2 | 4/4/1 | 306/17/4 | 66/65/12 | 212/34/6 | 5/5/0 | 58/42/4 |
| OR, 1e-7 / 1e-3 / min 2 | 0/0/0 | 282/16/3 | 39/39/6 | 193/27/6 | 4/4/0 | 50/38/3 |
| OR, fixed −3 dB hysteresis / min 2 | 13/13/1 | 323/81/18 | 168/161/19 | 204/86/17 | 10/10/1 | 113/55/5 |

†915 MHz spans can contain real ISM bursts.

- Recommended config in real spans: 0–2160 /MHz/h (zero-count bound 498 /MHz/h).
- Remaining boxes are mostly short (median 8 ms), 5–11 dB SNR, often at repeated frequencies (e.g. 94.665 MHz in both the 98 and 99 captures → at absolute frequency; 100.30 / 104.32 MHz at high gain). That fits weak intermittent real emissions or FM splatter beyond the ±50 kHz buffer rather than Gaussian false alarms, but 4 s can't separate them.
- OS-only has the fewest false alarms but misses the interior of wide flat signals (frame coverage of the 96.5 MHz station 44–56 % vs 100 % for OR).

### 3.4 Emitter-level detections and flags (non-edge, 16 MHz span)

| Capture | Emitters | Suppressed (reason) | Raster stations | Station parts (IBOC/split) | Comb | Unexplained (/MHz) | Marginal | Known FM 7 |
|---|---|---|---|---|---|---|---|---|
| 98M low | 3 | 2 (DC, 100.000 spur) | 0 | 0 | 0 | 1 (96.000 line) | 33 % | **0/7** (quantisation-limited; frame coverage 101.3 = 99 %) |
| 98M mid | 22 | 2 (DC, 100.000) | 13 | 3 | 0 | 4 (0.25) | 41 % | 7/7, 0 suppressed |
| 98M high (clipped) | 38 | 2 (DC, 100.000) | 13 | 5 | 13 (14 flagged) | 5 (0.31) | 61 % | 7/7, 0 suppressed |
| 99M mid (retune) | 24 | 2 (DC, 100.000) | 13 | 4 | 0 | 5 (0.31) | 42 % | 7/7, 0 suppressed |
| 915M low | 1 | 1 (DC) | – | – | 0 | – | 0 % | – |
| 915M high | 11 | 2 (910.005 spur, DC) | – | – | 0 | 9 unverified | 55 % | – |

- Flagged fractions — 98M mid: spur 4.5 %, DC 4.5 %, image 0, IMD 0. 98M high: spur 2.6 %, DC 2.6 %, comb 37 %, image 0, IMD 0, compressed 5.3 %, clipped 100 %. 915M high: spur 9.1 %, DC 9.1 %, compressed 9.1 %.
- "Unexplained" FM-band emitters (persistent, gain-linear, stay on retune, narrow 24–34 kHz): 91.10, 100.44, 102.78 MHz, plus weak split parts near 93.75 and 106.1 MHz — real RF at absolute frequency (local RFI) or reference-locked internal lines; only an antenna-off/terminated capture can tell.
- 915M high survivors: three bursty ISM emissions (908.80, 913.13/913.19 MHz; duty 1 %, block spread 34–42 dB); a 27 dB CW line at 921.0 MHz with bursty ±77 kHz sidebands; CW lines at 914.375 MHz (exactly −625 kHz = −fs/32 from the LO — suspected LO-relative spur, untestable without a 915 MHz retune), 911.98 and 909.97 MHz.
- Frame-level box totals (primary config): 356 / 2429 / 2484 / 2609 / 82 / 769. Continuous FM fragments into many 2 ms-frame boxes, so emitter/track-level aggregation is essential. Box flag counts in `metrics.json`.

### 3.5 Ghost tests

Gain step, real captures (`results/gainstep_rows.csv`, `figs/fig2_gainstep_test.png`):

| Step | G nominal (amp = 11 dB) | G_lin measured (anchors) | Δfloor | Real-ΔSNR bound | linear | compressed | suspect_imd | inconclusive weak / bursty |
|---|---|---|---|---|---|---|---|---|
| 98 low → mid | 26 | 26 (0 anchors → nominal) | 5.7 | 20.3 | 11 | 1 | **0** | 6 / 4 |
| 98 mid → high (clipped) | 29 | **33.0** (11) | 30.5 | 2.5 | 33 | 2 (DC, 100 MHz spur) | **0** | 0 / 3 |
| 915 low → high | 55 | 55 (0 → nominal) | 23.9 | 31.1 | 3 | 1 | **0** | 4 / 3 |

- **The amp is not 11 dB:** G_lin implies ~15 dB at 98 MHz; the sweep implies ~10 dB averaged over 1–1000 MHz.
- The 100 MHz spur grew only +20 dB for a +33 dB step — spurs have non-unity gain slopes; run the spur mask *before* gain-step inference.
- The quantisation-limited low gain widens the bound to 20–31 dB, leaving most weak emitters inconclusive.

Semi-synthetic IMD control (`results/imd_control.json`):

| a | G_lin | suspect_imd | Predicted IM3 (92.5, 100.3) found / flagged | Real raster stations flagged | Notes |
|---|---|---|---|---|---|
| 0 (negative control) | 10.0 | **0** | 0 / 0 | 0/11 | |
| 0.9 | 6.3 (carriers compress) | 8 | **2 / 2** | **0/11** | All 8 are 3rd-order products involving the injected carriers: 2f₁ − f₂ (92.5, 100.3), with the 100 MHz spur (95.4), with the DC offset (92.2), and three-signal f₁ + f₂ − f₃ (100.6, 102.6, 98.7, 103.9). ΔSNR of the IM3 pair 21 dB vs ideal 20. |
| 3.0 | 23.8 (cubic over-driven, gain inversion) | 2 | 2 / 1 | 0/11 | The test degrades once compression corrupts G_lin and Δfloor — reduce gain, don't infer. |

Retune 98 → 99 MHz (same gain): 19 stay, 1 moves with LO (DC), 2 not reproduced (both ~4.9 dB SNR, marginal); 99 MHz view 19 / 1 / 1.
- The 100.000 MHz spur stays at absolute frequency: the retune test cannot catch reference harmonics, nor RF IMD (also at absolute frequency). It catches LO-relative spurs, baseband IM2 (±Δ) and images (2Δ).
- No images detected: the strongest station is 20 dB SNR, so images at IRR > ~14 dB fall under the +6 dB seed; IRR is not measurable from this scene.
- The 96.000 MHz line at low gain (mirror of 100.000, IRR only 6.4 dB) is not an image: it does not grow with gain and vanishes at mid gain — a gain-independent internal line at −2 MHz (−fs/10) visible only in the quantisation-limited regime.

IM3 prediction: 0 real ghosts to test; 30 products from the 6 strongest stations cover 5.6 % of the span by chance.

### 3.6 Effect of overload (98M mid → clipped high)

- Floor +30.5 dB for +33 dB measured gain — no excess floor rise; clipping distortion stays below the floor. The spectrum is dominated by aggregate FM-band power (−6.3 dBFS mean).
- Emitters 22 → 38: the +16 are the 209.48 kHz comb (13–14 lines) plus weak lines; all pass SNR invariance (ΔSNR ≤ +5.5 dB ≈ removal of the 8-bit quantisation share of the mid floor) — revealed, not created.
- Known stations: SNR +1 to +4 dB, all still detected, none flagged.
- DC and 100 MHz spur compressed by 30 and 13 dB relative to linear.
- OS per-cell tail at design 1e-6: 10× → 110×. Quiet-span false boxes (per-frame floor config) 0 → 13; false tracks 0 → 3.
- `clipped` set on 100 % of detections, correctly.

**Conclusion:** this level of overload corrupts **statistics and levels** (tails, compression) rather than creating discrete ghosts; the clip flag and `compressed` catch it.

### 3.7 Survey level (sweeps; `results/sweep_metrics.json`, `figs/fig5_sweeps.png`)

| | 1–1000 MHz, 24/20/off | 1–1000 MHz, 32/40/on | 1–6 GHz, 32/30/on |
|---|---|---|---|
| Detections | 138 | 208 | 227 |
| n × 10 MHz spur-mask hits (chance coverage) | **37 (2.0 %)** | 29 (2.0 %) | 40 (9.1 %, too coarse to be meaningful) |
| Persistent (≥ 80 % of sweeps) | 39 | 63 | 86 |
| Known FM 7 | 7/7 | 7/7, none flagged | – |

- Reference harmonics are the dominant sweep spur family: 37/138 mid-gain detections sit on n × 10 MHz vs 2 % expected by chance; a regular line set is visible at 850–940 MHz.
- Gain step between sweeps: G_nominal 39, G_lin 38.1 (12 anchors), Δfloor 32.9, bound 5.3 dB. Verdicts: 89 linear, 8 compressed, **10 suspect_imd** (5 in 470–698 MHz), 101 inconclusive-bursty. **0/10** suspects coincide with predicted IM2/IM3 products (2.7 % chance). 99 detections are new at high gain; 8 of those flagged. Above ~200 MHz low-gain sweep sensitivity was ADC-limited (Δfloor < G_lin), so higher survey gain genuinely revealed weak emitters; below 200 MHz the high-gain trace rises relative to linear. Half the sweep detections are intermittent over 2.5 s, so sequential two-gain sweeps are weak evidence.
- 1–6 GHz: one gain only, so no ghost test. The floor steps at ~2.74 GHz (HackRF RF-path switch) and is structured over 1–2.7 GHz; block FCME follows it, but the power table K(f) and floor blocks must respect path boundaries.

## 4. What 4 s captures can and cannot establish

- **Can:** CFAR/floor calibration shape at 10⁻²–10⁻⁵ per cell (2–5 M cells per capture); persistent artefact inventory (spurs, DC, comb, CW lines); retune and gain-step behaviour of continuous emitters; known-emitter retention; relative overload effects.
- **Cannot:** a false-alarm rate near 1 /MHz/h (needs ≥ 3 MHz·h of quiet exposure for a 95 % bound, e.g. 10 MHz for 18 min; here 0.006 MHz·h); diurnal or impulsive-noise statistics; bursty-emitter gain steps (captures seconds apart); ghost suppression in a genuinely intermod-heavy scene; internal-vs-external origin of unexplained lines (needs antenna-off); anything about other locations or antennas.

## 5. Recommendations

### T-005 noise floor (`hk-dsp`)
- **Primary:** block FCME per SpectrumFrame — block 256 bins (~1.25 MHz at 4.88 kHz bins), hop 64; T_CME from Pfa 1e-3 using the Gamma(n_avg) quantile, **not** the exponential one; init 10 % smallest; iterate to convergence; truncated-mean bias correction P(n+1, nT)/P(n, nT); interpolate block centres in dB. Use the **per-frame** floor as the detection reference (it absorbs impulsive frames); its time-median or slow IIR for the science series and the integrated detector.
- **Cross-check/fallback:** block p20 with Gamma bias correction, valid only at occupancy < 40 %. Never p50 in dense bands.
- **Minimum statistics per bin:** only for drift tracking and idle-time floors of intermittent channels; never as a CFAR reference for continuous carriers (+4 to +14 dB bias measured). Window from the band profile (≥ several × the longest transmission), bias from Monte Carlo for the actual smoothing.
- **Uncertainty:** report ±0.5 dB (real estimator disagreement ≤ 0.3 dB; synthetic ≤ 0.01 dB) → SNR wall / measurability limit near −9 dB SNR.
- **Gain and quantisation state:** key every estimate by gain state; add a `quantisation_limited` provenance bit when the floor is within 3 dB of this unit's quantisation floor (−120.3 dBFS/Hz at 20 Msps, 4096 bins, Hann). Gain control should keep the floor ≥ 6 dB above it (mid gain here was only +5.2 dB).
- **Bootstrap:** per-frame FCME needs no detections, breaking the C08↔C09 loop; idle-time refinement comes after detection.

### T-006 CFAR + Detection records (`hk-detect`)
Defaults at 20 Msps: nfft 4096, n_avg 10, frame 2.05 ms.

| Parameter | Default | Evidence |
|---|---|---|
| Detector | OS-CFAR across frequency **OR** floor-referenced branch against the per-frame FCME floor | OS alone loses wide-signal interiors (44–56 % coverage); a static-floor branch explodes under impulsive frames |
| OS reference / guard / rank | N 32 (16 per side), G 4 per side, k 24 (3N/4) | Pfa within 1.15–1.5× on ideal and 8-bit synthetic noise |
| α | Numeric for Gamma(n_avg) order statistics. On: Pfa 1e-6 → **4.70 dB**. Off: Pfa 1e-3 → **3.00 dB** | Validated vs closed form and Monte Carlo |
| Floor branch | On T(1e-6) = **5.15 dB**; off T(1e-3) = **3.55 dB**; 3 dB guard on the OS branch | |
| Hysteresis | Off threshold from its **own Pfa (1e-3)**, never a fixed −3 dB | Fixed −3 dB: 35.5 false/MHz/h on ideal noise vs 0 (UB 0.91) |
| Min duration | **3 frames (~6 ms)** on the raw component before gap merge; 4-connectivity | 0 false boxes in 3.31 MHz·h |
| Short-burst profile | on 1e-7, off 1e-3, min 2 frames (~4 ms) where 2–4 ms bursts matter (ISM) | 0 in 1.32 MHz·h |
| Gap merge | 2 frames, after the duration test; no frequency merge at box level | |
| Impulsive-frame gate | Flag a frame when band-median P/floor rises > 0.5 dB above its running median; boxes mostly inside flagged frames become one broadband `impulsive` event; flagged frames don't update floors | Removed 24/37 mid-gain quiet-span boxes with a static floor |
| Emitter confirmation | A Detection becomes an emitter/track candidate only if it repeats (≥ 2 boxes at consistent frequency) or shows in the integrated (≥ 1 s) spectrum at seed +6 / extend +3 dB | Isolated-box false-alarm rates are 10²–10³ /MHz/h in real noise |
| Acceptance testing | Assert the false-alarm bound on synthetic noise and terminated-input fixtures; real-scene FA is a monitored metric, not a CI gate | Real per-cell tails 10–110× design |

### Spur, ghost and image rules (C05 → C09)
1. **Reference-harmonic mask (mandatory):** narrow (≤ 25 kHz) within max(10 kHz, 25 ppm·f) of n × 10 MHz → `spur_candidate` (reason `ref_harmonic`). The retune test cannot find these. Don't apply at sweep bin widths ≥ 250 kHz (coverage too large).
2. **DC/LO:** ≤ 40 kHz wide within 15 kHz of fc → `spur_candidate` (reason `dc`); retune confirms.
3. **Terminated-input spur map per gain state** (C05 bootstrap, before M0 field use): several persistent lines pass every in-capture test (91.10, 100.44, 102.78, 911.98, 914.375 MHz; the 96.000 MHz low-gain line). Antenna-off is the only discriminator.
4. **Comb rule:** ≥ 6 narrow lines on an arithmetic grid (±1.5 kHz, spacing 100 kHz–2 MHz, Monte-Carlo chance < 5 %) → `spur_candidate` (reason `comb`); keep visible as local RFI, don't delete.
5. **Image:** ≥ 20 dB stronger emitter at 2fc − f with mirrored-shape correlation > 0.5 → `image_candidate`; confirm by retune (moves 2Δ); measure IRR per unit with a signal-generator tone (this scene couldn't).
6. **Gain step (SNR invariance):** ΔSNR > max(0, G_lin − Δfloor) + 6 dB → `suspect_imd`; level growth < G_lin − 6 dB → `compressed`. G_lin from ≥ 3 anchors, never nominal; the step must include LNA/amp. Skip when the lower state is quantisation-limited or block SNR spread > 6 dB. Interleave gain states within one dwell (e.g. 0.5 s A / 0.5 s B × 3) so bursty emitters are comparable — a C04 scheduling requirement. Never run gain-step inference on clipped blocks; reduce gain first (a = 3 control).
7. **Retune:** Δ = 1 MHz (≥ 200 bins). Same absolute frequency → keep; ±Δ → `lo_relative`; 2Δ → `image_candidate`; not reproduced → `marginal`.
8. **Clip:** per-frame clip fraction > 1e-4 → `clipped` on every overlapping Detection, and trigger a gain-down (C05 dynamic-range rule).

### Provenance → Detection flag propagation (docs/07 §2.9)

| Flag | Rule |
|---|---|
| `clipped` | Any frame in [t_start, t_end] with clip fraction > 1e-4 (from Provenance / frame metadata) |
| `spur_candidate` | Add a reason code: `ref_harmonic`, `dc`, `lo_relative`, `comb`, `spur_map` (with SpurMask version) |
| `image_candidate` | Add a `retune_confirmed` bit |
| `marginal` | Peak SNR < 10 dB, **or** floor `quantisation_limited`, **or** any trust test inconclusive, **or** band-edge zone |

Recommended additions to §2.9: `suspect_imd` (already in the C05/C09 cards, missing from docs/07), `compressed`, `impulsive`, `edge`.

Provenance must carry: LNA/VGA/amp state; the measured G_lin table reference (amp gain is frequency-dependent: ~15 dB at 98 MHz, ~10 dB mean over 1 GHz); clip fraction per frame; floor estimate + uncertainty + `quantisation_limited`; `spur_mask_ref`; references to the gain-step/retune test results used.

### Preselector / FM notch
- **Don't move the switched preselector earlier on this evidence:** no discrete intermod ghosts appeared at up to +33 dB over mid gain with this antenna and location.
- **Move the FM notch purchase earlier** (cheap 88–108 MHz band-stop): docs/09 S4 explicitly needs the notch A/B (the missing half of this spike), and the measured overload was ADC clipping from aggregate FM-band power (−6.3 dBFS mean), which a notch addresses directly.
- **Re-run as S4b before T-006 thresholds are frozen:** outdoors or with a better antenna near strong transmitters; interleaved gain steps in one dwell; 50 Ω terminated capture per gain state; 915 MHz retune; ≥ 60 s captures; FM notch A/B; a second-gain 1–6 GHz sweep.
- **Decision rule:** if S4b shows suspect_imd > 10 % of emitters or known-emitter loss at usable gain, promote the preselector.

## 6. Reproduce

```sh
cd spikes/s4-detection-overload
python3 synth_check.py                       # alpha, Pfa (ideal + 8-bit), floor bias vs occupancy  (~2 min)
python3 synth_chain_fa.py 60                 # chain false-box rate, all configs, 1.32 MHz·h each (~3 min)
python3 synth_chain_fa.py 150 or_on1e-6_off1e-3_min3,os_only_on1e-6_off1e-3_min3 _long   # 3.31 MHz·h
python3 analyze_iq.py                        # six IQ captures: floors, detections, flags, tests, FA (~1 min)
python3 imd_control.py                       # semi-synthetic IMD positive/negative control (~1 min)
python3 sweep_survey.py                      # hackrf_sweep two-gain survey
python3 plots.py                             # figs/*.png (needs cache/ from analyze_iq.py and sweep_survey.py)
```

- Environment: Python 3, numpy 2.3.5, scipy 1.16.3, matplotlib 3.10.6 — spike-only tools, not product dependencies (no ADR-0010 ledger entry needed).
- Inputs: store captures in §1 (paths hard-coded in `s4lib.STORE`).
- Git-ignored (regenerated): `cache/` and the 644 KB `results/frame_boxes_primary.csv`.
- Files: `s4lib.py` (PSD, estimators, CFAR, masks); `analyze_iq.py`, `imd_control.py`, `sweep_survey.py`, `synth_check.py`, `synth_chain_fa.py`, `plots.py`; `results/` (`metrics.json`, `emitters_<capture>.csv`, `gainstep_rows.csv`, `imd_control.json`, `sweep_metrics.json`, `sweep_detections.csv`, `synth_check.json`, `synth_chain_fa.json`, `synth_chain_fa_long.json`); `figs/fig1_fm_three_gains.png`, `fig2_gainstep_test.png`, `fig3_cfar_calibration.png`, `fig4_impulsive_frames.png`, `fig5_sweeps.png`.
