# 21 — Do the ADR-0015 evidence bits survive 8-bit quantisation and gain? (T-547 measurement)

**Status:** measurement, 2026-09-21. Not a decision. Written for [ADR-0022 §9](adr/0022-false-confirm-budget.md), which
reserves the decision (an explicit amendment) for later and says in terms that its own
soundness does not depend on this result.
**Answers:** [ADR-0015 §2.2](adr/0015-decoder-synthesis-contracts.md)'s single unverified line —
"whether these tails are stable enough across 8-bit quantisation and gain states is **unverified**".
**Use cases:** RESEARCH-001, RESEARCH-008, SIGNAL-052.

## Verdict, in one paragraph

**CONDITIONAL.** Gain state on its own moves the calibrated tails by **nothing at all** — a
control that applies 51 dB of gain but skips the ADC reproduces the float table to within
0.02 bits on every metric (§4). What moves them is the **ADC**, and specifically the **fill
level**: a receiver whose noise sits at **σ ≈ 0.2 LSB** over-claims 1.0–1.6 bits on most
metrics and destroys one of them outright, while a receiver clipping **28 % of its samples**
over-claims ≤ 0.34 bits. The conditioning key ADR-0022 should name is therefore **ADC fill,
not gain and not clip fraction** — and the data model already carries it
(`Provenance.quantisation_limited`, `overload`, plus the per-capture clip count). With fill
conditioned (σ ≥ 0.5 LSB), the measured spread is **δ = 1.8 bits at a claimed 6 bits**
(95 % CI ≈ ±0.6) and **δ = 3.3 bits at a claimed 8** — δ is *not* a constant, it grows with
the claim, and the measurement cannot resolve whether the law is additive or multiplicative.

Three findings fell out that matter more than δ and are **not** about the receiver at all
(§5): `snr` and `evm` as the M1 FSK block computes them are the *same statistic* (Spearman
ρ = −0.989), so summing them as independent bits double-counts; `eye_open`'s null has an atom
that makes its quantile table **non-invertible at 4 and 6 bits**; and a calibration table
sampled with N windows can express at most **log₂ N bits**, which puts a hard price on
ADR-0015 §1.3's 12-bit caps.

## 1. What was measured, and how

The ladder metrics with **no analytic null** (ADR-0015 §2.2's second list), taken from the real
M1 implementations wherever one exists:

| ADR-0015 `MetricId` | source | direction |
|---|---|---|
| `bimodality` | `hk_classify::features` → `if_local_bimodality` (Sarle's coefficient of the instantaneous frequency about its local trend), on the mixed-and-filtered channel | larger = evidence |
| `eye_open` | `hk_demod::fsk::FskDemod` → `FskLock::eye_opening` | larger |
| `timing_var` | same → `FskLock::timing_rms_ui` | smaller |
| `snr` | same → `FskSymbols::symbol_snr_db` | larger |
| `evm` | harness, from that demodulator's soft symbols: `rms(abs(s) − median abs(s)) / median abs(s)` | smaller |
| `line_violations` | harness: share of disjoint bit pairs that are `00` or `11` | smaller |
| `bit_structure` | harness: Wald–Wolfowitz runs-test `abs(z)` over the sliced bits | smaller |

The last three have no block that publishes them yet; their definitions are the harness's own
and are stated here so the numbers can be reproduced or disputed.

**Corpus.** Synthetic IQ from `just synth --datatype cf32_le`, so the float reference really is
float and the ADC is applied by the harness exactly as `hkpy.synth.scene.Scene.quantise` does
(`round(clamp(g·x·127, −128, 127))/127`). Evaluation window 16 384 samples at 500 kHz with a
11 667-sample box and noise pads, i.e. **112 symbols of support** at 4800 Bd — `Evidence.n`
in ADR-0015's terms.

| corpus | scenario | n windows per condition |
|---|---|---|
| Null A — pure noise | `noise_floor_rise`, `step_db=0`, `n_weak_signals=0`, 140 s | **4200** |
| Null B1 — right block, **wrong symbol rate** (3200 Bd against a 4800 Bd burst) | `fsk_burst_train`, 20 dB | **820** |
| Null B2 — right block, **wrong centre** (+25 kHz) | same | **820** |
| Alternative — matched hypothesis | same, 20 dB and 8 dB | 820 each |
| Real — five HackRF captures, four distinct gain states | `fixtures/hackrf/2026-09-13` | 730 each |

**Gain grid.** Eleven ADC states plus float, spanning noise σ from **0.21 LSB** (under-filled)
to **77 LSB / 28 % clipped** (overloaded) on the noise corpus, and to **104 LSB / 76 % clipped**
on the signal corpora.

**Bits.** For a metric with a calibration table built on the float IQ of a null, the *claimed*
`b` bits is the raw value at that null's `1 − 2^−b` quantile. The **realised** bits under a
condition is `−log₂ P(raw exceeds that threshold | condition)`. **δ = claim − realised**, positive
meaning the table lies high. Windows the stage could not evaluate at all stay in the
denominator as the least-evident outcome — a demodulator that fails is not a missing sample,
it is a non-detection, and dropping it would inflate every tail.

## 2. δ — the number ADR-0022 asked for

Per-null table (each null calibrated on its own float IQ, as ADR-0015 §2.2 already requires),
worst over the ADC states with **σ ≥ 0.5 LSB**:

| metric | δ @ 6 b, Null A | Null B1 | Null B2 | **worst** | δ @ 8 b, worst |
|---|---|---|---|---|---|
| `bimodality` | +0.27 | +1.75 | +0.32 | **+1.75** | +2.32 |
| `eye_open` | (table degenerate, §5.2) | +0.64 | +0.49 | **+0.64** | +2.02 |
| `timing_var` | +0.32 | +0.78 | +0.71 | **+0.78** | +1.49 |
| `snr` | +0.15 | +1.57 | +0.57 | **+1.57** | +2.23 |
| `evm` | +0.11 | +1.18 | +1.57 | **+1.57** | +3.32 |
| `line_violations` | +0.19 | +1.71 | +1.71 | **+1.71** | +1.78 |
| `bit_structure` | +0.34 | +0.64 | +1.49 | **+1.49** | +2.32 |

**δ = 1.8 bits at a claimed 6 bits; δ = 3.3 bits at a claimed 8.** The Null-A column
(n = 4200) carries ±0.2 bits; the Null-B columns (n = 820) carry ±0.4 to ±0.9 bits, because a
6-bit claim puts only ≈ 13 windows in the tail and an 8-bit claim only ≈ 3.

**What this measurement cannot decide.** Whether δ is additive in the claim (δ ≈ 0.75 b per
claimed bit) or whether the realised significance saturates (worst realised went 4.25 b → 4.68 b
as the claim went 6 b → 8 b) is **not resolvable at n = 820**: both laws sit inside the interval.
Deciding it needs ~30 000 mismatched-null windows, not 820. Until then the honest reading is the
conservative one: **do not admit a calibrated claim above 6 bits per metric**, because above
that the discount is unmeasured rather than small.

## 3. The 8-bit question, asked directly

| noise σ (LSB) | clipped | demod success | δ @ 6 b, worst metric, Null A |
|---|---|---|---|
| **0.21** | 0 % | 53.2 % | **+1.64** (`bit_structure`); `eye_open` collapses to 0.91 b realised for a 6 b claim |
| 0.57 | 0 % | 20.4 % | +0.34 |
| 1.0 – 16 | 0 % | 18.5 – 19.4 % | ≤ +0.34 |
| 32 | 0.01 % | 18.8 % | ≤ +0.34 |
| 48 | 1.6 % | 18.1 % | ≤ +0.32 |
| 62 | 9.1 % | 17.2 % | ≤ +0.34 |
| **77** | **28.4 %** | 16.5 % | **≤ +0.34** |

The expected suspect — clipping — is **refuted**. Heavy clipping barely moves these statistics,
which is what hard-limiting theory predicts for an angle-modulated signal in noise (a limiter
costs about 2 dB and destroys no timing structure); the matched 20 dB signal read
`eye_open` 0.975 at 76 % clipped against 0.976 on float, and `snr` 21.0 dB against 21.7 dB.
**Under-fill is the real hazard.** At σ = 0.21 LSB the discriminator sees a mostly-zero stream,
the power gate behaves differently, the demodulator's success rate jumps from 19 % to 53 %, and
the null's tail fattens. The visible mechanism is not "noise added by quantisation" but
"the metric is measuring a different thing".

Note how far this is from the boundary in practice: of the five real captures, **four sit at
σ = 2.0–2.4 LSB** and one at 43.6 LSB. None is under-filled. The under-fill hazard is real but
the HackRF's own AGC-less gain range does not obviously put you there — one cheap runtime check
(`σ < 0.5 LSB`) covers it.

## 4. The control that makes this attributable

The same 4200 noise windows, with the same 51 dB of gain applied but the **ADC skipped**
(`--no-quant`), claim 6 bits:

```
metric        float  -11.1   -5.1    0.9    6.9   13.0   19.0   25.0   31.1   34.6   37.1   40.0
bimodality     5.99   6.01   6.01   6.01   6.01   6.01   6.01   6.01   6.01   6.01   6.01   5.99
eye_open       2.41   2.41   2.41   2.41   2.41   2.41   2.41   2.41   2.41   2.41   2.41   2.41
timing_var     5.99   5.99   5.99   5.99   5.99   5.99   5.99   5.99   5.99   5.99   5.99   5.99
snr_db         5.99   5.99   5.99   5.99   5.99   5.99   5.99   5.99   5.99   5.99   5.99   5.99
evm            5.99   5.99   5.99   5.99   5.99   5.99   5.99   5.99   5.99   5.99   5.99   5.99
line_viol      5.97   5.97   5.97   5.97   5.97   5.97   5.97   5.97   5.97   5.97   5.97   5.97
bit_struct     5.99   5.99   5.99   5.99   5.99   5.99   5.99   5.99   5.99   5.99   5.99   5.99
```

Identical, and the demodulator's success rate is 0.1876 in **every** column. Every metric in
the set is a ratio or a power-normalised statistic, so gain is exactly a no-op on float IQ; the
0.02-bit wobble on `bimodality` is one window moving across the threshold under `f32` rounding.
**ADR-0015's phrase "across gain states" names the wrong variable.** It should read *across ADC
fill states*.

## 5. Three findings that are not about the receiver

### 5.1 `snr` and `evm` are one statistic, not two

On the matched 20 dB corpus, Spearman ρ over 820 locked windows:

```
       bimodal  eye_ope  timing_   snr_db      evm  line_vi  bit_str
bimodal    1.000    0.003    0.045    0.021   -0.024   -0.107    0.128
eye_ope    0.003    1.000   -0.009   -0.013    0.028    0.019   -0.054
timing_    0.045   -0.009    1.000   -0.519    0.524   -0.014    0.108
snr_db     0.021   -0.013   -0.519    1.000   -0.989    0.059   -0.056
evm       -0.024    0.028    0.524   -0.989    1.000   -0.075    0.066
line_vi   -0.107    0.019   -0.014    0.059   -0.075    1.000   -0.706
bit_str    0.128   -0.054    0.108   -0.056    0.066   -0.706    1.000
```

`snr` ↔ `evm` at ρ = −0.989 is not a coincidence of this corpus: `FskDemod` derives
`symbol_snr_db = 10 log₁₀(a²/var)` and the soft symbols from the *same* `(a, var)` pair, so EVM
is a deterministic monotone map of symbol SNR. `line_violations` ↔ `bit_structure` at −0.706 are
both statistics of one bit sequence, and `timing_var` ↔ `snr` at −0.52 is the Gardner loop's
error shrinking with SNR. ADR-0015 §1.3 sums `b_j` over stages, and the stage caps hide some of
this, but **within** a stage these are not independent bits. The 12-bit S0–S3 caps are doing
more work than they look like they are, and a sum of correlated significances is not a
log-probability of anything.

### 5.2 `eye_open`'s quantile table is not invertible at 4 and 6 bits

Under the noise null, `eye_open` takes only **66 distinct values** over 4200 windows, and an atom
sits exactly where the 4-bit and 6-bit quantiles fall: asking the table for a 6-bit threshold
yields one that is really worth **2.41 bits**. At 8 bits it behaves (7.95 b realised). This is a
property of the statistic at this support, not of the receiver — the float reference is wrong by
3.6 bits before any ADC is involved. A quantile table must therefore be **built with its
achievable levels checked**, and a block must be able to say "I cannot express 6 bits here".

### 5.3 A table of N windows cannot express more than log₂ N bits

With n = 4200, the single most extreme window bounds the significance at **12.04 bits**
(`bit_structure`, which has a 5-window atom at its extreme, stops at 9.71). ADR-0015 §1.3 caps
S0–S3 at 12 bits and S4 at 32. The 32 is unreachable by calibration in principle; the 12 needs
**≥ 4096 windows per (block, support n, fill bucket) cell** just to touch once, and ~40 000 for
an estimate with a usable interval. With the fill conditioning of §3 that is a real cost on
`synth/calibration/<block>.json`, and it should be budgeted in M-2 rather than discovered there.

## 6. Real captures — what they add and what they cannot settle

Five `fixtures/hackrf/2026-09-13` captures at four gain states, run through the identical
harness at the identical samples-per-symbol (the symbol rate is scaled with `fs`, so nothing is
resampled and the recorded 8-bit samples are used exactly as captured), scored against the
**synthetic float noise table** at a claimed 6 bits:

| fixture | gain | σ (LSB) | `bimodality` | `eye_open` | `timing_var` | `snr` | `evm` | `line_viol` | `bit_struct` |
|---|---|---|---|---|---|---|---|---|---|
| ism 433.62 MHz | l24 g30 a1 | 2.33 | 5.93 | 2.45 | 5.93 | 6.34 | 6.93 | 6.51 | 5.93 |
| ism 915 MHz | l24 g30 a1 | 2.37 | 6.51 | 2.12 | 5.60 | 5.81 | 5.81 | 5.05 | 5.51 |
| urban 98 MHz | l24 g20 a0 | 2.05 | 6.51 | 1.49 | 4.76 | 4.93 | 5.05 | 4.60 | 5.05 |
| fm 100.8 MHz | l32 g30 a1 | 23.40 | 5.51 | 2.23 | 5.81 | 5.12 | 5.70 | 5.26 | 5.70 |
| urban 98 MHz | l32 g30 a1 | 43.56 | 7.93 | 0.90 | 4.56 | 4.76 | 5.34 | 4.19 | 4.05 |

n = 730 each, so each entry carries roughly ±0.45 bits. Setting `eye_open` aside (§5.2), a
6-bit claim realises **4.05 to 7.93 bits** on real 8-bit HackRF data — an over-claim of up to
**1.95 bits** and an under-claim of up to 1.93, a ~3.9-bit range that is **wider than the
synthetic gain sweep produced**.

**This number cannot be attributed.** The "quiet channel" of a real capture is not pure noise:
it carries the front end's spurs and intermodulation, LO leakage, and the skirts of whatever
was actually on the air, none of which the synthetic corpus contains. The real-capture spread
therefore measures *synthetic-table-versus-real-world*, which is quantisation **and** gain
**and** environment together, and nothing here separates them. ADR-0022 should keep it as a
**separate allowance** and not fold it into δ. The only way to split it is a **50 Ω terminator
capture** at several gain states — the receiver's own noise with no sky — which is already
[T-375](tasks.yaml), still waiting on the user.

## 7. What this corpus cannot tell you

- **Intermodulation and spurs.** Simulable in principle (`hkpy.synth.impairments` has a blocker
  and an IM3 coefficient) but not simulated here, and a city's real IMD environment is not
  reproducible at all. ADR-0022 §3.2's margin exists partly for this; nothing here shrinks it.
- **Other blocks.** Everything above is the **FSK** path plus one classifier feature. `am_demod`
  envelope bimodality, PSK EVM (there is no Costas block yet, ADR-0015 §1.1's catalogue gap) and
  the C4FM path are unmeasured. There is no reason to assume δ transfers.
- **Other supports.** All numbers are at **n = 112 symbols**. §5.2 and §5.3 are both explicitly
  support-dependent, so a table must be indexed by `n` — which ADR-0015 §2.2 already says ("at
  stated `n`"), and §5.2 shows is load-bearing rather than bookkeeping.
- **The alternative hypothesis.** δ here is a **null-side** quantity. The recall ADR-0022 hopes
  to buy depends on how the *signal* side moves, and at 8 dB SNR and σ = 0.26 LSB the matched
  metrics collapse (`eye_open` 0.19 against 0.899 on float). Under-fill costs recall far more
  than it costs validity.
- **Sample size.** No claim above 8 bits is measured at all, and §2's 8-bit row rests on ~3
  windows in the tail per condition. "0 exceedances in 820" bounds a rate only to 3.7 × 10⁻³.

## 8. What ADR-0022 should do with this

Read against [ADR-0022 §9](adr/0022-false-confirm-budget.md)'s table, this is the **CONDITIONAL**
row. Its consequence for T-575's `ConfirmPolicy`:

1. **Nothing changes without an explicit amendment.** `min_analytic_holdout_bits = 24`,
   `hard_check_floor_bits = 16`, `min_check_width = 8` and §4.2's formula all stand; none reads a
   calibrated metric. This note does not amend the ADR, deliberately: §9 says the amendment is a
   decision to be taken openly rather than "under pressure later", and a measurement ticket
   should not take it on the ADR's behalf.
2. **If the amendment is written**, the conditioning key to name is **ADC fill**
   (`quantisation_limited` / measured σ in LSB), not gain and not clip fraction (§3, §4).
3. **The discount is not a constant.** δ = 1.8 b at a 6-bit claim, 3.3 b at 8. A single δ is
   only safe if calibrated claims are also capped at 6 bits per metric, which is what §2
   recommends.
4. **Keep a separate real-world allowance** of ~2 bits for §6's unattributed spread, or gate the
   admission on T-375's terminator capture landing first.
5. **Calibrated bits still may not be summed as independent evidence** (§5.1), so any admission
   should take the **maximum** over correlated metrics within a stage, not their sum — this is
   the larger risk to the 24-bit total, and it is not a receiver problem.

## 9. Reproducing it

Harness: `crates/hk-pipeline/examples/t547_ladder.rs` (research tool; not on any product path,
not called by any test). Analysis: the two scripts named in the commit message, kept out of the
tree because they are single-use. Nothing under `fixtures/` was written.

```sh
just synth noise_floor_rise --seed 547 --out $D/noise --datatype cf32_le \
  --param sample_rate=500e3 --param duration_s=140 --param step_db=0 \
  --param n_weak_signals=0 --param noise_dbfs=-40 --param t0_s=1.0
just synth fsk_burst_train --seed 5471 --out $D/sig20 --datatype cf32_le \
  --param duration_s=42 --param period_s=0.05 --param jitter_s=0 \
  --param first_burst_s=0.05 --param snr_db=20

cargo run --release -p hk-pipeline --example t547_ladder -- \
  --iq $D/noise/noise_floor_rise.sigmf-data --windows 4200 --stride 16384 \
  --win 16384 --box-lo 2360 --box-hi 14027 --label noise
```

`--no-quant` gives §4's control; `--rate`/`--cfo` give the mismatched nulls; `--gains-db 0` with
an already-`ci8` stream converted to `cf32` gives the real-capture rows.

## Follow-ups filed

T-616 (`snr`/`evm` are one statistic — ADR-0015 §1.3 must not sum correlated metric bits),
T-617 (quantile tables must declare their achievable bit levels and their `n`),
T-618 (calibration-table sizing and the ADC-fill conditioning key for M-2),
T-619 (extend this measurement to the AM/OOK and C4FM paths before they ship an `evidence()`).
