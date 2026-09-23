# 21 — Do the ADR-0015 evidence bits survive 8-bit quantisation and gain? (T-547 measurement)

**Status:** measurement, 2026-09-21; **§10 extends it to the AM/OOK and C4FM paths at two
supports (T-619, 2026-09-22)**. Not a decision. Written for
[ADR-0022 §9](adr/0022-false-confirm-budget.md), which reserves the decision (an explicit
amendment) for later and says in terms that its own soundness does not depend on this result.
**§§1–9 are the FSK path at one support and every "every metric" in them means that path's
metrics**; §10 is where the other blocks are, and its first finding is that the two do not
agree.
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

## 10 — The AM/OOK and C4FM ladders, at two supports (T-619)

§7 said there was no reason to assume δ transfers to the other blocks. **It does not.** The same
harness, extended (`--path ook|c4fm`), measured the AM/OOK envelope ladder and the C4FM path over
the same gain grid, the same noise null and three signal-present nulls each, at **two supports**.
Three sentences carry the result:

1. **Gain is still a no-op** — the no-ADC control reproduces the float table to ≤ 0.02 bits on
   both new paths, as it did on the FSK path (§10.6). The conditioning key is the ADC, on all
   three paths measured.
2. **§3's headline is path-specific and must not be generalised.** "Clipping is refuted,
   under-fill is the hazard" is a statement about an **angle-modulated** signal read by an
   angle-demodulating ladder. The AM/OOK ladder measures *amplitude*, and clipping is amplitude
   distortion: at 77 % clipped its metrics over-claim **+5.1 to +6.0 bits** on a 6-bit claim
   (§10.3). Both ends of the ADC range are hazards there, not one.
3. **δ grows with support.** At the larger support every path's worst δ is larger, not smaller
   (OOK +0.46 → +1.85, C4FM +1.16 → +1.02/+1.37 depending on the metric). A calibration table
   indexed by `n` is not bookkeeping; a table borrowed from a shorter window **under-states** the
   discount.

**Verdict:** **C4FM is CONDITIONAL** (small δ, but two of its six metrics must not ship an
`evidence()` at all). **AM/OOK is NO-GO under [ADR-0015 §13.3](adr/0015-decoder-synthesis-contracts.md)'s
current `nominal` bucket** and CONDITIONAL under a tighter one. §13.5's own trigger for splitting
that bucket — "any null-side measurement beyond 28 % clipped" — is exactly what fired.

### 10.1 What was measured

| path | S1 | S2 | S3 | source |
|---|---|---|---|---|
| `ook` | `am_demod` | `clock_recovery` (nrz, Gardner) | `slicer` | the real M1 blocks, built through `Registry::build`, so their parameters pass the same schema validation a recipe's do |
| `c4fm` | — | `hk_demod::fsk::c4fm::C4fmDemod` (one block spans S1–S3) | — | the real C23 path |

Metrics, and where each comes from:

| path | metric | source | ADR-0015 `MetricId` |
|---|---|---|---|
| ook | `env_bimodality` | harness: Sarle's coefficient of the `am_demod` envelope (the formula `hk_classify` applies to the IF) | `bimodality` — §1.1's "AM/OOK: envelope bimodality" |
| ook | `env_depth` | `am_demod` `Status.extra["depth"]` | — |
| ook | `gamma_max`, `mu42_a` | `hk_classify::features` (the Azzouz–Nandi envelope pair) | — |
| ook | `eye_open`, `snr_db` | `clock_recovery` `Status.quality` / `Status.snr_db` | `eye_open`, `snr` |
| ook | `timing_var` | rms of `clock_recovery`'s `timing_error` diagnostic port | `timing_var` |
| ook | `evm`, `line_viol`, `bit_struct` | harness, §1's definitions, over the `slicer`'s output | `evm`, `line_violations`, `bit_structure` |
| c4fm | `if_local_bimodality`, `if_local_modality` | `hk_classify::features` | `bimodality` |
| c4fm | `level_margin` | `C4fmSymbols::level_margin` (mean distance to the sliced ideal level: the 4-level EVM) | `evm` |
| c4fm | `offset_ratio` | `C4fmSymbols::residual_cfo_hz` / `outer_deviation_hz` | `offset_ratio` |
| c4fm | `dibit_balance` | harness: Wilson–Hilferty \|z\| of the dibit histogram against uniform | `line_violations` analogue |
| c4fm | `bit_struct` | harness: runs test over the dibits expanded MSB-first | `bit_structure` |

**Corpora** (all `cf32_le` from `just synth` at 500 kHz, ADC applied by the harness, as §1):

| role | ook | c4fm |
|---|---|---|
| Null A — noise | `noise_floor_rise`, step 0, 140 s (§1's corpus, reused) | the same |
| Null B1 — **wrong symbol rate** | `acars_message` read at 1600 Bd, true 2400 | `trunk_control_channel` read at 3200 Bd, true 4800 |
| Null B2 — **wrong centre** | the same, +25 kHz off | the same, +25 kHz off (an empty 12.5 kHz raster slot) |
| Null B3 — **wrong family / wrong emitter** | the §1 2-FSK burst corpus, i.e. a constant-envelope emission read by an envelope ladder | the scene's unframed 4FSK **decoy** channel: right modulation, wrong emitter |
| Alternative — matched | `acars_message` at 2400 Bd on its centre (AM, 70 % depth, keyed) | the C4FM control channel, 100 % duty |

**Supports.** Window 16 384 and 65 536 samples, stride = window. Median realised support:
**77 and 313 symbols** (ook, 2400 Bd), **156 and 628 dibits** (c4fm, 4800 Bd). Windows per
condition: 4200 / 1068 on the noise null, ~3670 / ~915 on the signal nulls (the 2-FSK corpus is
shorter: 1281 / 320). A 6-bit claim therefore puts ≈ 66 / 17 windows in the tail, so these δ
carry roughly **±0.35 bits at the small support and ±0.7 at the large**; the 8-bit columns carry
±0.7 and ±1.4 and are indicative only.

**Direction.** Each metric's evidence tail is fixed **once per (path, metric)** from the float
rows of Null A against the matched alternative, and then used for every null — the alternative
defines which tail is evidence, not the null under test.

### 10.2 Per-metric verdicts

δ at a 6-bit claim, worst over all four nulls and over the admitted ADC states, at both supports.
"ADR bucket" is [§13.3](adr/0015-decoder-synthesis-contracts.md)'s `nominal` (σ ≥ 0.5 LSB,
clip ≤ 30 %); "tight" is σ ≥ 1.0 LSB, clip ≤ 10 % (§10.4). **Recall** is the share of matched
windows that beat the Null-A 6-bit threshold — validity is worthless without it.

**AM/OOK** (n = 77 / 313 symbols):

| metric | δ ADR bucket | δ tight | δ@8 tight | recall | verdict |
|---|---|---|---|---|---|
| `env_bimodality` | +3.01 / +5.06 | +0.46 / +0.47 | +0.49 / +0.68 | 0.87 / 0.95 | **CONDITIONAL** on the tight bucket |
| `gamma_max` | +3.12 / +4.93 | +0.13 / +0.80 | +0.74 / +0.94 | 0.95 / 1.00 | **CONDITIONAL** — the strongest AM metric |
| `mu42_a` | +1.46 / +4.33 | +0.29 / +0.26 | +0.49 / +1.26 | 0.95 / 1.00 | **CONDITIONAL** |
| `env_depth` | +1.10 / +3.87 | +0.38 / +1.85 | +0.48 / +3.26 | 0.19 / 0.90 | **CONDITIONAL**, useless at the short support |
| `eye_open` | +2.97 / +2.14 | +0.14 / +0.49 | +0.32 / +0.96 | 0.41 / 0.64 | **CONDITIONAL**, and identical to `snr_db` (§10.5) |
| `snr_db` | +2.97 / +2.14 | +0.14 / +0.49 | +0.32 / +0.96 | 0.41 / 0.64 | **not a second metric** |
| `timing_var` | +1.35 / +3.13 | +0.26 / +0.26 | +0.96 / +1.75 | 0.58 / 0.98 | **CONDITIONAL** |
| `evm` | +1.65 / +1.85 | +0.36 / +0.26 | +0.85 / +1.47 | 0.40 / 0.57 | **CONDITIONAL** |
| `line_viol` | −0.01 / +0.32 | −0.01 / +0.26 | −0.00 / +1.15 | 0.29 / 0.48 | **NO-GO at 6 bits**: 23 distinct values at n = 77, table not invertible (§10.6) |
| `bit_struct` | +2.38 / +3.86 | +0.27 / +0.68 | +0.36 / +1.61 | 0.46 / 0.75 | **CONDITIONAL** |

**C4FM** (n = 156 / 628 dibits):

| metric | δ ADR bucket | δ tight | δ@8 tight | recall | verdict |
|---|---|---|---|---|---|
| `if_local_bimodality` | +0.43 / +1.37 | +0.15 / +0.80 | +0.42 / +1.16 | 1.00 / 1.00 | **GO** at the short support, CONDITIONAL at the long |
| `level_margin` | +0.27 / +0.40 | +0.23 / +0.40 | +0.62 / +0.75 | 0.99 / 1.00 | **GO** — the best-behaved calibrated metric measured on any path |
| `dibit_balance` | +0.39 / +0.52 | −0.11 / +0.52 | +0.42 / +0.52 | 0.80 / 1.00 | **CONDITIONAL**: +2.38 once clipping is unconditioned, and it has atoms |
| `if_local_modality` | +2.23 / +3.65 | +1.16 / +1.02 | +1.33 / +1.33 | 0.99 / 1.00 | **NO-GO**: a mode count over 67 distinct values cannot express 6 bits (a 6-bit ask realises 6.15, an 8-bit ask 9.71) |
| `offset_ratio` | +0.11 / +0.52 | +0.11 / +0.52 | +0.66 / +1.40 | **0.036 / 0.080** | **NO-GO**: carries no evidence (§10.3) |
| `bit_struct` | +0.43 / +0.75 | +0.33 / +0.75 | +0.13 / +1.62 | **0.006 / 0.009** | **NO-GO**: no separation in **either** tail |

### 10.3 Two metrics that are parameters, not evidence

`offset_ratio` and `bit_struct` on the C4FM path have small δ and near-zero recall: a matched
control channel is *less* likely than noise to produce an extreme value, in the tail the medians
point to. That is not a weak metric, it is a **mis-typed** one.

- **`offset_ratio`.** Against a noise null the discriminator's mean is zero by symmetry, so
  *noise* has the smaller residual offset and a real emission the larger. A residual carrier
  offset is a **parameter estimate**; it becomes evidence only against a template that states
  what the offset should be, which is `prior_bits`, not `evidence_bits`. ADR-0015 §1.1 lists
  "offset/deviation" under S1 primary evidence; on this path that line needs splitting.
- **The deviation estimate is the counter-example, and it is a prior.** `outer_deviation_hz`
  reads **4157 Hz median on noise and 1634–1776 Hz on every real 4-level emission**: *zero* of
  4200 noise windows land within ±10 % of C4FM's 1800 Hz, against 100 % of matched windows. It
  separates "a 4-level emission is here" almost perfectly — and separates the *right* emitter
  from the wrong one not at all (98.6 % of the wrong-rate null and 61.5 % of the decoy null also
  land inside the same ±10 %). Exactly the plausibility check §1.3 keeps out of `evidence_bits`.

### 10.4 The `nominal` bucket is not tight enough for either path

Worst δ at a 6-bit claim over all nulls and metrics, by admitted bucket:

| bucket | ook n = 77 | ook n = 313 | c4fm n = 156 | c4fm n = 628 |
|---|---|---|---|---|
| σ ≥ 0.5, no clip limit | +5.38 | +6.00 | +2.38 | +3.65 |
| **σ ≥ 0.5, clip ≤ 30 % (ADR-0015 §13.3 `nominal`)** | **+3.12** | **+5.06** | **+2.23** | **+3.65** |
| σ ≥ 1.0, clip ≤ 30 % | +1.11 | +4.79 | +1.16 | +1.02 |
| **σ ≥ 1.0, clip ≤ 10 %** | **+0.46** | **+1.85** | **+1.16** | **+1.02** |
| σ ≥ 2.0, clip ≤ 10 % | +0.36 | +1.14 | +0.33 | +0.80 |

Two things move it, and they are the two ends of the ADC range:

- **Clipping, on the envelope path only.** On the noise null clipping is as harmless as §3 found
  (≤ 0.52 bits at 28 % clipped). On the *signal-present* nulls it is not: at 77 % clipped
  `env_bimodality` over-claims **+5.12** and `gamma_max` **+5.38** at the short support, +5.77
  and +6.00 at the long. A limiter costs an angle-modulated signal about 2 dB and destroys no
  timing structure (§3); it costs an *amplitude*-modulated one the entire statistic. §3's
  refutation of clipping was a true statement about the FSK path and is a false one about this
  one. The C4FM path sides with FSK, as its physics says it should: its clipping damage is to
  **recall** (`level_margin` 0.99 → 0.88, `dibit_balance` 0.80 → 0.56 at 82 % clipped), not to
  validity.
- **Under-fill, worse than on the FSK path.** At σ = 0.21 LSB the worst OOK metric over-claims
  **+3.20 bits** at a 6-bit claim (FSK: +1.64; C4FM: +0.89 at the short support, +2.24 at the
  long). And the σ ≥ 0.5 boundary is measured on the *stream*, not on the noise: on the
  signal-present corpora the −11.1 dB state reads σ ≈ 0.6 LSB in total while its **noise floor is
  still at ~0.2 LSB**, and that is precisely where the OOK nulls break (+3.01 on `env_bimodality`
  at 0 % clipped). **The fill that matters is the noise floor's fill, not the window's.** A
  runtime rule reading `std(re)` over a window containing a strong emission will pass a window
  whose null behaviour is under-filled.

### 10.5 Dependence groups: one exact duplicate, one near one

Spearman ρ over the matched alternative at the short support (§5.1's measurement, repeated):

- **`eye_open` ↔ `snr_db` on the AM/OOK path: ρ = 1.000, exactly.** `clock_recovery` computes
  `snr_db = 10 log₁₀(q/(1−q))` from the same eye quality `q` it publishes — a deterministic
  monotone map, so they are one number twice. T-547 found ρ = −0.989 for the FSK path's pair and
  called summing them double-counting; here it is not an approximation. `evm` ↔ `eye_open` is
  −0.876 and `env_depth` ↔ `mu42_a` is 0.678 in the same matrix.
- **`level_margin` ↔ `offset_ratio` on the C4FM path: ρ = 0.842.** Both are derived from the
  single level-centring step (`centre` and the 80th-percentile `outer`), so a noisy centre moves
  both. Nothing else on that path exceeds 0.13.

[ADR-0015 §13.1](adr/0015-decoder-synthesis-contracts.md)'s maximum-within-a-declared-group rule
is therefore confirmed on two more paths, and the groups to declare are
`{eye_open, snr, evm}` on the AM/OOK path and `{evm(level_margin), offset_ratio}` on the C4FM
path. §13.1's rule survives; only its table changes, which is what §13.5 predicted.

### 10.6 Expressibility, and what it costs at each support

§5.2 found `eye_open`'s table non-invertible at 4 and 6 bits on the FSK path. The generalisation
is that **every statistic on a discrete support has atoms**, and its expressible levels are a
function of `n`:

| metric | distinct values (of 4200 / 1068 null windows) | realised for a 4 / 6 / 8-bit ask |
|---|---|---|
| `line_viol` (ook) | 23 at n = 77, 40 at n = 313 | 4.10 / **6.54** / 9.45 (short); 4.25 / **6.48** / 8.06 (long) |
| `if_local_modality` (c4fm) | 67 at n = 156, 168 at n = 628 | 4.36 / **6.15** / **9.71** (short) |
| `dibit_balance` (c4fm) | 243 / 548 | 4.00 / **6.54** / 8.04 (short) |
| `bit_struct` (c4fm) | 838 / 904 | 4.00 / **6.25** / 8.13 (short) |
| everything else | ≥ 3832 of 4200 | 4.00 / 5.99 / 8.04 |

Two differences from §5.2 worth keeping. First, **these atoms under-claim**, not over-claim: a
6-bit ask lands at 6.5 realised, which is conservative — unlike `eye_open`'s 2.41, which was a
3.6-bit lie. The refusal ADR-0015 §13.2 requires is still the right behaviour (a block must not
silently deliver 6.54 when asked for 6.00), but it is not a validity hole on these paths.
Second, the distinct-value count **roughly doubles with the support**, so `admissible_bits` is
per-`n` and a table cannot be shared across supports even where δ says it could.

### 10.7 The control: gain is still exactly a no-op

The §4 control, repeated on both new paths — 4200 noise windows, 51 dB of gain applied, the ADC
skipped, 6-bit claim:

```
ook    float  -11.1   -5.1    0.9    6.9   13.0   19.0   25.0   31.1   34.6   37.1   40.0
env_bimodality  5.99 ...  5.99 on every column
gamma_max       5.99   5.99   5.99   5.99   5.99   5.99   5.97   5.97   5.99   5.97   5.99
line_viol       6.54 ...  6.54 on every column (its atom, §10.6)

c4fm
if_local_bimod  5.99   6.01   6.01   6.01   6.01   5.99   6.01   5.99   6.01   6.01   6.01
level_margin    5.99 ...  5.99 on every column
if_local_modal  6.15   6.13   6.13   6.13   6.15   6.13   6.13   6.15   6.11   6.15   6.13
```

Three paths, 33 gain columns, zero movement beyond ±0.02 bits (the ±0.02 wobble is windows
crossing a threshold under `f32` rounding). §4's conclusion holds for every block measured so
far: **ADR-0015's "across gain states" names the wrong variable**, and naming ADC fill instead
was right — it is only the *boundaries* of the fill buckets that this note changes.

### 10.8 What this still cannot tell you

- **PSK EVM remains unmeasured**, because there is still no Costas block (ADR-0015 §1.1's
  catalogue gap). Nothing here transfers to it either: the two paths measured differ from each
  other more than either differs from the FSK path.
- **The AM alternative is an AM-MSK aviation waveform, not a hard-keyed OOK sensor.** Its envelope
  keys on and off between messages but carries a constant-envelope subcarrier while on, so the
  *recall* column for the AM/OOK path is pessimistic for a true OOK burst and the δ column —
  a null-side quantity — is unaffected. A hard-keyed 902–928 MHz OOK corpus would sharpen §10.2's
  recall, not its verdicts.
- **No real captures.** §6's unattributed real-world spread was measured only on the FSK path;
  nothing says it is the same ~2 bits here, and the AM/OOK path has the stronger reason to differ
  (a city's IMD environment is an *amplitude* phenomenon).
- **The 8-bit columns rest on ≈ 16 windows at the short support and ≈ 4 at the long.** They are
  indicative. §2's "do not admit a calibrated claim above 6 bits" is, if anything, better
  supported here than there.

### 10.9 Reproducing it

Harness: the same `crates/hk-pipeline/examples/t547_ladder.rs`, with `--path ook|c4fm`
(`--path fsk` is T-547's, unchanged but for one added `"path"` key per output line). Analysis: single-use scripts named in the commit
message. Nothing under `fixtures/` was written.

```sh
just synth trunk_control_channel --seed 619 --out $D/trunk --datatype cf32_le \
  --param sample_rate=500e3 --param duration_s=120
just synth acars_message --seed 619 --out $D/acars --datatype cf32_le \
  --param sample_rate=500e3 --param n_bursts=350 --param period_s=0.345 --param prekey_s=0.05

cargo run --release -p hk-pipeline --example t547_ladder -- \
  --iq $D/trunk/trunk_control_channel.sigmf-data --path c4fm \
  --rate 4800 --dev 1800 --cfo 37500 --win 16384 --stride 16384 --windows 4200 --label alt
cargo run --release -p hk-pipeline --example t547_ladder -- \
  --iq $D/acars/acars_message.sigmf-data --path ook \
  --rate 2400 --cfo 50000 --ch-bw 12000 --win 65536 --stride 65536 --windows 4200 --label alt
```

The nulls are the same commands with `--rate`/`--cfo` moved, `--iq` pointed at the noise corpus,
or `--no-quant` for §10.7. The whole sweep — 22 runs, 3 corpora, ~2.5 M window-evaluations — took
about 40 minutes on 8 threads.

### 10.10 What ADR-0022 and ADR-0015 should do with this

Nothing changes without an explicit amendment (§8.1's rule, unchanged). When one is written:

1. **The `nominal` fill bucket must split, and §13.5 said so in advance.** Its stated trigger —
   "any null-side measurement beyond 28 % clipped" — fired: at 77 % clipped the AM/OOK nulls
   over-claim 5–6 bits. The measured bucket that holds every path to ≤ 1.2 bits at a 6-bit claim
   is **σ ≥ 1.0 LSB and clip ≤ 10 %**; at σ ≥ 2.0 and clip ≤ 10 % it is ≤ 1.14. Whether to split
   by path or to tighten globally is the decision, and the cheap answer is to tighten globally:
   it costs recall only where the receiver is already misconfigured.
2. **Fill must be measured on the noise floor, not on the window** (§10.4). The §13.3 runtime
   rule (`σ_LSB < 0.5` over the evaluation window) passes windows whose *null* behaviour is
   under-filled whenever a strong emission is present — which is exactly when a ladder runs.
3. **Two C4FM metrics must not ship an `evidence()`:** `offset_ratio` (a parameter, not evidence)
   and `bit_struct` (no separation in either tail). `if_local_modality` must not claim 6 bits.
   On the AM/OOK path, `eye_open` and `snr` are one number and must be one dependence group.
4. **Tables are per support, and the discount grows with `n`.** Nothing measured here lets a
   table be shared across supports, in either direction.

## Follow-ups filed

T-616 (`snr`/`evm` are one statistic — ADR-0015 §1.3 must not sum correlated metric bits),
T-617 (quantile tables must declare their achievable bit levels and their `n`),
T-618 (calibration-table sizing and the ADC-fill conditioning key for M-2),
T-619 (extend this measurement to the AM/OOK and C4FM paths before they ship an `evidence()`).

**T-616, T-617 and T-618 are answered by [ADR-0015 §13](adr/0015-decoder-synthesis-contracts.md)**
(2026-09-21): within a stage, declared dependence groups contribute their **maximum**, not their
sum, and undeclared means one group; a calibration table declares its expressible levels, its
`N` and its `admissible_bits`, and **refuses** a level it cannot express instead of returning
the nearest quantile; tables are conditioned on **ADC fill** in two measured buckets
(σ ≥ 0.5 LSB and clip ≤ 30 % gets a table, everything else gets none and scores 0 bits), at a
budgeted 4 096 windows per cell. That amendment deliberately leaves
[ADR-0022](adr/0022-false-confirm-budget.md)'s confirm gate untouched — §8 above recommends
the maximum rule to it, and §2.1 of that ADR had already excluded every calibrated metric from a
confirm, so §5.1's finding costs search recall and display honesty, not validity.
**T-660** carries the Rust conformance test and the generator, which need `hk-synth` to exist.
**T-619 is answered by §10 above** (2026-09-22): δ does not transfer, the AM/OOK ladder is NO-GO
under §13.3's current fill bucket and CONDITIONAL under a tighter one, C4FM is CONDITIONAL with
two of its six metrics ruled out of `evidence()` entirely, and the bucket-splitting trigger
ADR-0015 §13.5 wrote down has fired. PSK EVM stays unmeasured until a Costas block exists.
