# 22 — The MAUTO acceptance-fixture corpus: what it contains, and how it knows it is enough

**Status:** SPECIFICATION, frozen 2026-09-21 (T-551). Written **before** the synthesis engine exists, by an agent that will not implement it — which is the entire point of the ticket. Every threshold below is fixed *a priori*; [ADR-0015 §7](adr/0015-decoder-synthesis-contracts.md)'s standing rule applies without exception: **the M-12 review may tighten these, never loosen them after seeing results.**
**Serves:** [ADR-0015 §7](adr/0015-decoder-synthesis-contracts.md) (the evaluation table this extends), [ADR-0021 §8](adr/0021-search-trace-and-negative-result.md) (the negative-control suite and the false-**label** budget), [ADR-0022 §10](adr/0022-false-confirm-budget.md) (A1/A2/A3 and the false-**confirm** margin measurement).
**Reads on:** **T-547** / `docs/21-evidence-bits-under-quantisation.md` (on `task-t547`, not yet merged) — the measured evidence-bits ladder under 8-bit quantisation, which decides three of this corpus's axes; [docs/17](17-burst-recall-vs-open-set.md) §1 (the draw-spread lesson); [docs/10](10-test-strategy.md) (tiers); [docs/sigmf-extension.md](sigmf-extension.md) (`hackriff:truth`).
**Use cases:** SIGNAL-062, SIGNAL-001, SIGNAL-052, RESEARCH-002.
**Status of the ADRs it reads:** both PROVISIONAL. **This document changes no ADR's status and amends no ADR.**

> **Why a standalone doc rather than an ADR-0021 or ADR-0022 amendment.** The corpus is consumed by *both* ADRs and by ADR-0015 §7, and the enumerable part of it (a scene table with per-row truth, thresholds and trial counts) is the kind of artefact that grows every time a generator lands — amending either ADR would bury a shared, mutable table inside one ADR's argument and force a second amendment the moment the other ADR's suite reads it. This is the pattern docs/21 already used for T-547's measurement, and both ADRs get a pointer line instead.

---

## 0. The one-paragraph version

The corpus is **one fixture set serving three different claims**, and those claims need different populations, different trial counts and different statistics: a **recall** claim (the engine finds what is there), a **false-label** claim (it does not report structure in structureless input, ADR-0021 §8.3), and a **false-confirm margin** claim (the analytic nulls are not optimistic by more than ADR-0022 §3.2's 9.7 bits). The third is the one that cannot be demonstrated by counting confirms — §3 shows why, and shows that the *margin* measurement at n = 1600 has sensitivity to 7.4 bits of optimism, which is **2.3 bits tighter than the margin it protects**, so the CI tier is adequate for that claim while remaining 75× too weak for a rate claim. Sufficiency is argued axis by axis in §6, with an explicit statement of what a gap on each axis would look like **from outside the suite**, and with four axes marked **NOT ARGUED** rather than papered over. The corpus varies **ADC fill**, not gain, because T-547 measured that gain is exactly a no-op and fill is not; and every row that rests on a **calibrated** (rather than analytic) evidence bit in a family T-547 did not measure is **report-only, not a pass bar**, until T-619 lands.

---

## 1. Three claims, not one suite

| | Claim | Population | Statistic | Budget / bar | n | What it honestly bounds |
|---|---|---|---|---|---|---|
| **C-R** | Recall: the engine solves what is present | P + S rows (§4, §5) | pass rate per cell | ADR-0015 §7's bars | 400 draws / cell (§5.2) | a pass rate to ±0.02 (binomial) plus a reported between-base sd |
| **C-L** | False labels ≤ 1 / 1000 jobs | N1, N2, N4, N5 | `false_labels == 0` | ADR-0021 §8.3 | 800 / profile | a rate to ≤ 3.7 × 10⁻³ (rule of three). **Not** 1/1000 |
| **C-M** | The analytic nulls are not optimistic by > 9.7 bits | the same negatives | `max analytic_holdout_bits` | ADR-0022 §10.1 A2 | 1600 pooled | **optimism > 7.4 bits** (§3). This is the claim with power |

They share fixtures and share a runner. They do **not** share a trial count, and reporting them as one "the suite passed" line is the defect ADR-0022 §10.2 warns about. The suite emits three verdicts.

**A fourth thing the suite must do and no budget covers: not be passed by doing nothing.** ADR-0022 §10.1 A3 is the recall control for C-L/C-M. §6.4 adds two more degeneracy detectors, because A3 alone is satisfied by an engine that solves the four famous protocols and nothing else.

---

## 2. The axes

The corpus is a **stratified sample of an axis product**, not a full crossing. Full crossing is ~10⁵ jobs and buys less than the stratification does (§5.2). The axes, and why each is an axis rather than a fixed value:

| # | Axis | Levels | Why it is an axis |
|---|---|---|---|
| **A1** | modulation family | 2-FSK, 4-FSK/C4FM, OOK/ASK, MSK, CSS (LoRa), analog FM voice, analog AM voice, CW, OFDM, DSSS, 16-QAM, thermal noise | the engine's block catalogue is per-family; T-547 measured **only FSK** (§7) |
| **A2** | SNR (in the emission's own bandwidth) | 6, 10, 20 dB; plus **8 dB** as the T-547 corner where matched metrics collapsed | ADR-0015 §7's bars are indexed on it |
| **A3** | **ADC fill**, σ of the noise in LSB | 0.21 (under-filled), 0.6, 2.0 (nominal — where 4 of 5 real captures sit), 23, 43, 77 / 28 % clipped | **T-547's finding.** Gain with the ADC skipped reproduces the float table to 0.02 bits; fill does not. A corpus that swept gain would have swept a no-op |
| **A4** | support: symbols per evaluation window | 112 (T-547's only support), 512, 4096; and **burst** (isolated, ≤ 4 frames) vs **continuous** | docs/17: a genuinely short emission is short in the analyser's window too; T-547 §5.2/§5.3 are both support-dependent |
| **A5** | catalogue membership | template-fixed / in-catalogue-but-searched / out-of-catalogue structure / structureless | ADR-0022 §5's `L_check` is zero on one of these and ~21 bits on another; the same scene must be run both ways |
| **A6** | CFO | 0, ± 0.2 × bandwidth | ADR-0015 §7 |
| **A7** | check | CRC-8 / 16 / 24 / 32 template-fixed; the same searched; a **random polynomial not in the RevEng catalogue**; BCH(31,21); **no check**; **constant payload** | ADR-0022 §4.2/§4.3 turn width and `differences` into the confirm gate; the constant-payload row is the one that must *fail* to confirm |
| **A8** | templates on / templates off | both, for every scene | ADR-0015 §7. **Reported separately and never averaged** (§8) |

**Not axes, deliberately:** transmit gain, LNA/VGA/amp setting (A3 subsumes them — T-547 §4), and wall-clock (jobs are bounded by `max_evaluations`, ADR-0021 §8.4).

---

## 3. What the false-confirm margin measurement actually needs — the arithmetic that sizes the negatives

ADR-0022 §10.2 states that A1 (`confirms == 0`) at n = 800 bounds the rate only to 3.7 × 10⁻³ against a 5 × 10⁻⁵ budget, and moves the confidence to **A2**, the maximum analytic hold-out bits reached on input with nothing to find. A2's *own* resolution has to be stated too, and ADR-0022 does not state it. It is the single number that sizes this corpus:

> **A sample of n negative decisions can resolve an exceedance probability no finer than 1/n, i.e. a tail location of log₂(n) bits.** The observed maximum is therefore an estimate of the **log₂(n)-bit quantile**, never of the 24-bit gate.

| n | finest resolvable tail | A2 assertion `max ≤ 18` fires when the true optimism exceeds |
|---|---|---|
| 800 (per profile) | 9.6 bits | **8.4 bits** |
| **1600 (pooled, the CI tier)** | **10.6 bits** | **7.4 bits** |
| 20 000 (nightly tier) | 14.3 bits | 3.7 bits |
| 60 000 (ADR-0022 §10.2's number) | 15.9 bits | 2.1 bits |
| 262 144 | 18.0 bits | — (probes the constant directly) |
| 16.8 M | 24.0 bits | — (probes the gate directly) |

Three consequences, all of which the corpus is built around:

1. **n = 1600 is the right CI size for C-M, and it is not a compromise.** ADR-0022's margin `M` is 9.7 bits; the budget is missed only when the realised optimism exceeds 9.7. A2 at n = 1600 fires at 7.4. The test is therefore **conservative by 2.3 bits** — it complains before the budget breaks — which is the correct asymmetry on a one-way door. The 60 000 run ADR-0022 §10.2 hands to T-576 is needed for the **rate** claim (C-L/A1), not for the margin claim, and the two should not be conflated when someone decides whether to build it.
2. **The `18.0` constant is a function of n and must not be frozen independently of it.** At n = 1600 it means "optimism ≤ 7.4 bits". At n = 200 the same constant means "optimism ≤ 10.3 bits" — *above* the margin, i.e. **no power at all**, while still reading green. The corpus therefore freezes **n, not only the constant**, and the suite asserts against `log2(n_actual) + 7.4`, reporting both. A suite that silently shrinks its negative population is otherwise indistinguishable from a suite that passes.
3. **The extrapolation from 10.6 bits to 24 bits must be visible, not hidden inside a `max`.** The corpus requires the negative runs to report the **upper-tail slope**: the fitted line of `log₂(exceedance)` against claimed bits over the top decade, with its interval. A slope of −1 is the nominal model; a slope of −0.6 says the tail is 40 % fatter than log-linear and the extrapolation to 24 is worth far less than the `max` suggests. This is reported, never asserted (we have no a-priori slope to fix), and it is the quantity a future n = 60 000 run would sharpen most.

---

## 4. The scene table

Columns: **Gen** = how it is produced (`have` = a generator or capture in the repo today; `build` = needs work, ticket named in §9). **Currency** = whether the row's pass depends on **analytic** evidence (sync excess, check width, field fit) or on **calibrated** metric bits — see §7's rule. **Bar** = a pass threshold; **report** = a measurement recorded for the M-12 review with no bar (ADR-0015 §7's own instruction for anything that cannot be justified before results).

### 4.1 P — positives (recall; claim C-R)

| id | scene | Gen | truth carried | A5 | Currency | assertion | n |
|---|---|---|---|---|---|---|---|
| **P1** | RDS on `fm_100p8M` (real capture) | have | PI/PS from the `hk_demod::rds` oracle | template-fixed + open | analytic | templates-on `solved`, rank-1 `rds`, confirmed at `standard`; templates-off ≥ `framed` with the 26-bit block period found | 1 fixture × 8 windows |
| **P2** | POCSAG 4-channel net (T-095) | have | pages, per channel | both | analytic | on: `solved`, ≥ 95 % of truth pages on hold-out, confirmed. off: `solved`, sync `0x7CD215D8` + BCH(31,21) recovered | 4 channels × 3 seeds |
| **P3** | ACARS (T-098) | have | label/text + CRC | both | analytic | on: `solved`, CRC valid on hold-out. off: ≥ `clocked` | 3 seeds |
| **P4** | ADS-B squitters (SIGNAL-001) | have | ICAO + fields per squitter | template-fixed | analytic | ≥ 14/16 singles `solved`; burst set confirms; **≥ 1 single squitter confirms on one frame** (ADR-0022 §4.2) | 16 singles × 3 seeds |
| **P5** | 915 MHz FSK sensor (T-078) | have | rate, sync, CRC-16, payloads | open only | analytic | `solved` or `checked`; symbol rate within 1 % | 3 seeds |
| **P6** | **CRC-8 emitter in 902–928, template-fixed** | **build** (A7 width param) | 3 differing payloads | template-fixed | analytic | **confirms at 3 differences** — the case ADR-0015 §5.5's width ≥ 16 refused outright (ADR-0022 §10.1 A3) | 3 seeds |
| **P7** | **constant-payload beacon** (same bytes every frame) | **build** (A7 payload param) | the repeated payload | template-fixed | analytic | **does NOT confirm** on repeat count alone; `differences` = 1 while `distinct_valid` ≫ 1 | 3 seeds |
| **P8** | C4FM trunking control channel (T-268 family) | have | TSBKs, grants, band plan | both | **calibrated-leaning** | **report only** until T-619 (§7) | 3 seeds |
| **P9** | LoRa/CSS ISM burst | have | SF/BW/CR/payload | out-of-catalogue today | calibrated-leaning | **report only** (§7; T-851 measured CSS, docs/21 §11: stays report-only); asserted only as "never `solved`", which is its N3 role | 3 seeds |

P6 and P7 are the two rows that exist *because* ADR-0022 changed the gate; without them the new policy is untested in exactly the two places it differs from the old one.

### 4.2 S — the generic sweep (the only rows that measure real synthesis)

ADR-0015 §7's sweep row, made exact. **Templates-off only**, by construction.

| id | family | rate | sync | check | SNR | CFO | fill | bar |
|---|---|---|---|---|---|---|---|---|
| **S1** | 2-FSK | log-uniform 300 Bd – 50 kBd | random 16–32 bits, uniform | uniform over {CRC-8, CRC-16} from the RevEng catalogue | stratified 6 / 10 / 20 dB | uniform ±0.2 × BW | 0.75 nominal, 0.15 high/clipped, **0.10 under-filled** | `solved` ≥ 80 % @ 20 dB, ≥ 60 % @ 10 dB, ≥ 50 % at least `framed` @ 6 dB |
| **S2** | **OOK/ASK** | as S1 | as S1 | as S1 | as S1 | as S1 | as S1 | **report only** at first freeze (§7): no generator exists and no ladder measurement covers the family |

**S2 is the half of ADR-0015 §7's "FSK/OOK sweep" that has never existed.** The row has been written as if both halves were available since ADR-0015; only FSK is. It is named here so M-12 is sized honestly (§9, T-621).

### 4.3 N — negatives (claims C-L and C-M)

ADR-0021 §8.4's four populations, **plus a fifth this document adds** and justifies.

| id | population | Gen | n (CI tier) | must return |
|---|---|---|---|---|
| **N1** | thermal noise at the fixture's gain state | have (`noise_floor_rise`, `n_weak_signals=0`) | 400 | `unknown` / `no-signal`, `deepest_verdict: energy`; 0 labels ≥ `framed`; 0 emitters |
| **N2** | energy without symbols: CW, analog FM voice, analog AM voice | partly (`tone` = CW; **build** the voice scenes) | 200 | `unknown` / `nothing-scored`, `deepest_verdict ≤ demodulated`; 0 ≥ `framed` |
| **N3** | out-of-catalogue structure: OFDM (non-standard CP), DSSS, 16-QAM, CSS | **build** 3 of 4 (CSS = P9) | 200 | `unsupported-structure` naming the structure + `missing_block`; 0 `solved`; `framed` only where the trace shows a *measured* framing. **Scored separately** |
| **N4** | real empty capture: the 433 MHz quiet window + **the 50 Ω terminator capture** | partly — **T-375 is a user action, outstanding** (§10) | 200 | as N1, and **a spur is never a label** (`artifact_of`, never a verdict ≥ `demodulated`) |
| **N5** | **mismatched-hypothesis null: a real signal whose true parameters lie outside the proposal grid**, and a strong adjacent emitter leaking into the analysed box | **have** (`mismatched_hypothesis`, T-626) | 200 | `unknown` / `tied`, or a *correct* partial result; **never `solved` with wrong parameters**; over-claim counted here |

**Why N5 exists and ADR-0021's four populations are not enough.** T-547 measured the null the search actually faces, and it is not noise. On pure noise the 6-bit tables over-claimed **≤ 0.34 bits**; on a **mismatched-parameter** null (right block, wrong symbol rate) they over-claimed **up to 1.75 bits** — five times worse, and that is before an adjacent real emitter is in the window. A negative suite built only from noise, unmodulated energy and out-of-catalogue structure measures the engine against the *friendliest* null available and would report a comfortable margin while the operational null was five times fatter. N5 is the population where C-M's `max analytic_holdout_bits` is most likely to move, and it is pooled into the C-L and C-M counts.

**Built (T-626, 2026-09-22).** `hkpy.synth.mismatched_hypothesis`, two shapes selected by
`--param population=`:

- `off_grid` (default) — a real, framed, **CRC-valid** 2-FSK emitter at **1873 Bd** and
  **h = 1.281**. Against the declared proposal grid (300 … 38 400 Bd; h ∈ {0.5, 1.0, 2.0}) **0 of
  8** rates and **0 of 3** indices are within 10 % — the nearest are 2400 Bd (21.96 % away) and
  h = 1.0 (28.14 % away), so every hypothesis on the grid is the wrong one and something on the
  grid always scores best. The rate and index are re-measured out of the samples (discriminator +
  zero crossings: 1872.95 Bd) and the CRC re-checked with `binascii.crc_hqx`, so the off-grid
  property is in the IQ, not only in the truth block.
- `adjacent_leakage` — the same emitter plus a **hard-keyed** neighbour 30 kHz away (34 dB up,
  2 kBd, rectangular envelope → sinc skirts). It is never inside the analysed box, and an A/B
  against the `off_grid` control measures **+17.6 dB** inside the box from it, so any parameter
  bound from it is a mismatch by construction. A neighbour placed *inside* the box is refused by
  the generator: that is two overlapping emissions, which the signal model calls an error signal,
  not adjacent-channel leakage.

The truth carries `negative_population` with the grid, both distances, the analysed box, the
measured leak and the **mismatch-vs-miss rule** (`hkpy.synth.mismatch.classify_outcome`): a label
≥ `framed` with a bound symbol rate outside 2 % of the true one is a **mismatch**, and
`grid_snapped` records the aggravating case where it bound a grid value instead;
`unknown` (any reason) and a sub-`framed` partial are the **wanted** answers. A miss is explicitly
**not** counted as a false label — N5 measures over-claim, not recall, and conflating the two is
how a suite reports a comfortable margin by being bad at finding things.

`false_labels` is counted over **N1 ∪ N2 ∪ N4 ∪ N5** (1000 jobs per profile; N3 separately), and C-M's pooled max is taken over the same set at both profiles (**n = 2000**, finest tail 11.0 bits, A2 sensitivity **7.0 bits**). Both improve on §3's table; the frozen assertion stays keyed to `log2(n_actual) + 7.4` so the extra population cannot be spent as slack.

### 4.4 F — the ADC-fill ladder (T-547's axis, promoted from research to acceptance)

Six fill levels (A3) × {one P row, one N row}, **n = 200 per cell**, with a **comparative** claim only:

- **report** per fill level: C-R pass rate, C-M max bits, demod success, and `resolution.reason` distribution;
- **bar:** the *under-filled* cell (σ = 0.21 LSB) must be **detected and flagged**, not silently degraded — `Provenance.quantisation_limited` set, and the result carrying the flag rather than a confident answer. T-547 found under-fill swings demod success 19 % → 53 % and collapses `eye_open` to 0.19 from 0.899; an engine that reports the same confidence at both ends of that is over-claiming, and the flag is the only cheap defence.

**Built (T-625, 2026-09-21).** The ladder is `hkpy.synth.fill` — `python -m hkpy.synth.fill
<scenario> --out DIR [--levels ...]` — one `ci8` recording per rung, differing from its neighbours
in `adc_gain_db` and in nothing else, each carrying the fill it was *measured* at (never the one it
was asked for) in `hackriff:provenance.noise_sigma_lsb`, plus a `fill-ladder.json` manifest of
target σ / gain / measured σ / clip fraction / bucket. Two things it measured that the spec should
absorb:

- **The six rungs classify into two buckets, and `over_clipped` is unreached.** On
  `noise_floor_rise` at 200 ksps the rungs land at σ = 0.13, 0.67, 2.02, 23.1, 43.0, 70.4 LSB with
  clip fractions 0, 0, 0, 0, 0.007, 0.188 — **1 `under_filled`, 5 `nominal`, 0 `over_clipped`**,
  because ADR-0015 §13.3's clip boundary is 30 % and docs/21's sweep stopped at 28.4 %. The F
  ladder therefore exercises the *under-fill* refusal and never the *over-clip* one; a scene that
  exercises the second needs a rung past where the measurement went, and it would be a rung with
  no evidence behind it. Stated here rather than quietly padded.
- **The top rung's measured σ falls below its target** (70.4 for 77) precisely because it clips —
  the recorded value is post-ADC, which is the honest datum and the reason the manifest records
  measured rather than requested fill.

n = 200 per cell resolves a tail to 7.6 bits, which is **too coarse for an absolute C-M claim** — which is exactly why F is comparative and is *not* used to stratify the C-L/C-M populations. Stratifying the 1000-job negative set across six fill buckets would have cut the tail resolution from 11.0 bits to 7.4 and destroyed A2's power. **That trade is the reason fill is a separate population rather than a cross with the negatives**, and it is stated here so nobody "improves" the suite by crossing them.

### 4.5 H — the sealed hold-out

**20 % of every generated scene family, selected by a rule (`sha256(scene_id ‖ seed) mod 5 == 0`), is generated, truthed, and then not run** until the M-12 review. No one tunes against it because no one has seen it. §6.4 explains what it detects.

---

## 5. Generation, seeds and reporting

### 5.1 How fixtures are produced

Preference order, and it is not a style preference: **synthetic via `just synth` first**, because the truth is exact rather than adjudicated; **real capture** only where the point of the row is the real front end (P1, N4) or where no generator can produce the phenomenon (real IMD, real spurs — §6.3). Every real capture records gain state, antenna and σ-in-LSB per CLAUDE.md. Small fixtures in `fixtures/` (LFS); the sweep and negative corpora are **generated at run time from a frozen seed list** and never committed — 2400 sweep draws plus 2000 negative windows is not a thing to store.

**Fixtures drive the mock SDR behind the real device interface.** No row in this table feeds a file into the pipeline. Targets come from blind detection or an ad-hoc band; **no row looks a frequency up in the database and tunes there.**

### 5.2 Seeds, draws, and why 400

ADR-0015 §7's bars are pass *rates*, and a rate reported as a point estimate from a small draw is the error docs/17 §1 documents. T-428 measured a **draw sd of 0.0084** on a comparable held-out figure, i.e. the between-seed-base variation is real and is *not* captured by a binomial interval on a single base.

- **400 draws per (family, SNR) cell**, as **8 seed bases × 50 draws**. Binomial sd at p = 0.8 is 0.020; the between-base sd is reported separately and is the number that tells you whether a 0.81 is a pass or a coin flip.
- **The bar applies to the pooled point estimate.** The Wilson 95 % interval and the between-base sd are printed beside it. A run whose point estimate clears the bar but whose Wilson lower bound falls below it is a **PASS WITH A FLAG**, recorded in the M-12 review. This is not a loosening — the bar is unchanged — it is the difference between "passed" and "passed, and here is how close".
- **Seed policy:** the 8 base seeds are fixed constants in the suite, published in the report, and never resampled to chase a green. A re-run with different seeds is a *different measurement* and is labelled as one.
- **Tiering.** The full 2400-draw sweep is a **milestone/nightly** step. CI runs the fixed P and N rows (deterministic, cheap) plus a **100-draw sweep smoke** whose only assertion is the ≤ bound (0 over-claims), because a zero-failure bound is valid at any n while a pass-rate bar at n = 100 has sd 0.04 and would fail a good engine one run in nine.

### 5.3 Over-claim

ADR-0015 §7's "verdict above truth in ≤ 1 %" is asserted over S1 ∪ S2 ∪ N5 with the **decision rule written out**: `over_claims / n ≤ 0.01`, with the Wilson upper bound reported. At n = 2400 with zero over-claims the rule-of-three bound is **≤ 1.25 × 10⁻³**, comfortably supporting the 1 % claim; at n = 300 it is exactly 1 % and supports it only marginally. **The suite must print n beside the percentage**, every time.

### 5.4 Hidden truth

`hackriff:truth` per [docs/sigmf-extension.md](sigmf-extension.md), carrying per scene class: emission centre / bandwidth / time extent / family; symbol rate, deviation, sync word, check polynomial + width + start bit + bit order; per-frame payloads; and for negatives an explicit `{"kind": "none"}` or `{"kind": "unsupported", "structure": "ofdm", "missing_block": "..."}`.

**Enforcement is structural and already exists.** `tests/e2e/src/blind.rs` strips truth into a replay copy, **seals** the original and asserts (via atime, with a canary that self-tests whether the filesystem tracks it) that the system never opened it. Every row in §4 runs through that entry point. The rules, restated because they are binding on each row: truth is loaded **only** by the assert harness; thresholds are a priori (this document); **an artefact correctly flagged is a success, not a missed detection** (N4's spur rows assert `artifact_of`, and a run that *finds* the spur and names it passes).

---

## 6. Sufficiency: how the suite knows it is enough

This is the part that a corpus which is merely *present* fails. A list of scenes covers what someone thought of, and its gaps are invisible from inside it — the corpus cannot report an absence it does not represent. So sufficiency here is **not** "we covered everything". It is a three-part standard, applied per axis:

> **A corpus is sufficient on an axis when (a) the axis is represented by a population that could produce a failure, (b) the population is large enough that a failure of the size that matters would be *visible*, and (c) there is a stated, observable symptom of a gap on that axis that is visible from OUTSIDE the suite** — because (a) and (b) alone are satisfied by a corpus that is confidently wrong about what the axis's levels are.

Part (c) is the load-bearing one and it is where most test corpora have nothing to say.

### 6.1 The axis table

| Axis | Represented by | Power: a failure of what size is visible | **What a gap looks like from outside** |
|---|---|---|---|
| **A1 family** | P1–P9, S1–S2, N2, N3 | per-family pass rates at n ≥ 150 → sd ≤ 0.04 | **ADR-0021 §9.4's `missing_block` backlog is the instrument.** A family with no scene shows in the field as a family that is *never* `solved` and *always* `unsupported-structure` naming the same missing block. That backlog is a ranked, observable list; a name near its top with no row in §4 is a gap, visible without re-running anything |
| **A2 SNR** | stratified: every sweep draw is assigned an SNR level by design, not by chance | 400/level | **the marginal pass rate is flat in SNR.** An engine tested only in its comfortable region produces a flat curve; a real curve falls. A flat curve is the suite telling you it is not testing the hard end |
| **A3 ADC fill** | F ladder + 10 % of sweep draws under-filled | comparative Δ at n = 200/cell; ~240 under-filled sweep draws → sd 0.03 | **field confirms cluster on one gain state.** ADR-0022 §8's decision-rate counter already records the gain/fill state of each decision; a confirm distribution concentrated in one fill bucket while the survey spans several is the symptom |
| **A4 burst vs continuous** | P4 (burst), P7, N5 burst rows, support levels 112/512/4096 | 3 supports × ≥ 48 | **docs/17's exact failure, and it is already documented:** recall collapses for short emissions while the suite stays green because the suite's windows are long. The external symptom is the ratio of ephemeral to continuous emitters in the inventory falling far below the 902–928 MHz reality (bursts everywhere) |
| **A5 in- vs out-of-catalogue** | every scene run templates-on and templates-off (A8) | paired, so the difference is measured on the same draws | **the two columns converge.** If templates-on and templates-off report the same pass rate, either the library is empty or the search is not being used; if templates-off goes to zero, there is no real synthesis and the product is a decoder catalogue wearing a search's clothes. **This is why §8 forbids averaging them** |
| **A7 check / evidence currency** | CRC-8/16/24/32, template-fixed and searched, random polynomial, BCH, none, constant payload | P6/P7 are single-purpose rows for the two gate changes | **the confirm reasons in the field are all one shape.** ADR-0022 §11.3 stores `l_check`, `check_searched` and `template_provenance` on every `Decode`; a device-week whose confirms are 100 % template-fixed means the searched path has never confirmed and its threshold is untested by use |
| **A6 CFO** | uniform ±0.2 BW on every sweep draw | 2400 draws | a pass rate that is a step function of abs(CFO) |
| **A8 templates** | both, always | — | see A5 |

### 6.2 Degeneracy detectors: the suite must be able to fail

Coverage arguments are passed by suites that cannot fail. Four mechanisms, three of which already exist in the ADRs and are pulled together here:

1. **The recall control (ADR-0022 §10.1 A3).** A false-confirm suite with no recall control is passed by never confirming. Already mandatory.
2. **The null-control falsifiability clause (ADR-0021 §8.2).** If the shuffled-null control saves **zero** jobs across the whole suite, the mechanism is dead weight and §8.2 is to be reconsidered. Already mandatory; the corpus's job is to contain input on which it *could* fire — which is N5's other purpose.
3. **The expected-failure rows.** At least one row per family is placed **deliberately past the edge** (6 dB at 112 symbols) and is asserted to **fail honestly**: verdict below `framed`, `reason: nothing-scored`, no over-claim. A corpus with no expected-failure rows cannot distinguish a strong engine from a permissive threshold, because everything passes either way.
4. **The sealed hold-out (§4.5).** 20 % of scenes, chosen by hash, never run until M-12. If sealed-set performance differs materially from the open set, the open set has been **overfit** — which is precisely "the corpus covers what someone thought of", made measurable. The sealer is a rule, not a judgement, so it cannot be gamed by the same process that built the corpus.

### 6.3 Where sufficiency CANNOT be argued — marked, not papered over

Four axes. On each, the honest statement and the external symptom are given; none of them is closed by adding scenes.

**G1 — The unknown case. NOT ARGUABLE by construction.** You cannot sample the complement of the catalogue. N3 contains structures that are real, documented and *deliberately unimplemented*, which makes it a **held-out** set, not a sample of the unknown: it measures behaviour on **known unknowns only**. What the corpus does instead is turn the distribution itself into the instrument — the suite publishes the `resolution.reason` distribution per population as a **reference distribution**, and a field survey whose distribution diverges from it (e.g. `unsupported-structure` at 30 % where the suite predicts 3 %) is the only available detector for a real unknown-case gap. That is weaker than a coverage claim and is stated as weaker.

**G2 — The real IMD / spur environment. NOT SIMULABLE.** `hkpy.synth.impairments` has a blocker and an IM3 coefficient, but a city's real intermodulation environment is not reproducible, and T-547 §6 measured the consequence: real 8-bit captures scored against a synthetic table realised **4.05 to 7.93 bits for a 6-bit claim** — a 3.9-bit range **wider than the entire synthetic gain sweep produced** — and that spread **cannot be attributed** between quantisation, gain and environment. ADR-0022 §3.2's margin exists partly for this and nothing in this corpus shrinks it. External symptom: the margin measured on synthetic negatives (C-M) being systematically better than the same measurement on real captures.

**G3 — The calibrated-bit discount outside FSK. UNMEASURED.** §7.

**G4 — The rate claim itself.** C-L and C-M bound optimism and bound a rate to 3.7 × 10⁻³. **Nothing in this corpus demonstrates 5 × 10⁻⁵**, and §3 shows what would: 60 000 negative decisions, which is T-576's open question, not a gap this spec can close by choosing better fixtures.

### 6.4 The coverage manifest: gaps as empty cells, not as absences of thought

The strongest available defence against "the corpus covers what someone happened to think of" is to make the *cell space* an artefact rather than the *cell contents*. The suite therefore emits, on every run, a **coverage manifest**: the declared axis product from §2, and for each cell one of three marks —

- **populated** (n jobs ran, with the count),
- **declared unreachable** (with the reason and the ticket, e.g. "OOK: no generator, T-621"),
- **unmarked** — which is a **failure of the manifest**, not of the engine.

The third mark is the point. A cell nobody thought about shows up as unmarked; a cell somebody thought about and could not fill shows up as declared. The M-12 review reads the manifest before it reads any pass rate.

**Built (T-627, 2026-09-23).** `hk_e2e::corpus` holds the axes of §2, the declared planes (the recall grid A1×A2×A4, A1 against each other axis, and an `EF` expected-failure plane — the stratified crossings the argument is made over, not the ~10⁵-cell full product) and the three marks; `acceptance_mauto::mauto_corpus` declares the rows and the unreachable regions, fails on any `UNMARKED` cell, checks every `populated` row names a test that exists in the binary and is not ignored, checks every declaration cites an **open** ticket (a done ticket means the gap is now fillable; no owner is spelled `UNFILED`, never a plausible wrong id), and diffs the rendered manifest against the committed `tests/e2e/mauto-coverage-manifest.txt` (re-bless with `HK_BLESS_MANIFEST=1`). The hold-out is `sha256(utf8(scene_id) ‖ be_u64(seed)) mod 5 == 0`, unsealed only by `HK_MAUTO_UNSEAL=M-12`. The expected-failure rows (2-FSK, C4FM, MSK at 6 dB / 112 symbols) drive the real `structure::measure` + C4FM framing blocks at component tier, each with a 20 dB / 4096-symbol control that must clock; they move behind the device interface when T-567 makes the engine write a sealed `nothing-scored` resolution. **First manifest: 492 cells, 19 populated, 473 declared, 0 unmarked** — the corpus is overwhelmingly declared-empty today, which is the honest reading. This is the same discipline as the canvas's coverage map: **grey is genuinely unobserved, and it is the point** — an honest empty cell beats a silently absent one, and `Coverage::Unobserved` is not `quiet`.

---

## 7. What T-547's blind spots mean for this corpus

T-547 measured the evidence-bits ladder on **the FSK path plus one classifier feature, at one support (112 symbols)**, and was explicit that `am_demod` envelope bimodality, PSK EVM (no Costas block exists) and the C4FM path are **unmeasured**, with "no reason to assume δ transfers". Three consequences, all binding on the freeze:

**7.1 The fill axis exists because of T-547, and the gain axis does not.** A control applying 51 dB of gain with the ADC skipped reproduced the float table to **0.02 bits on every metric with a bit-identical demod success rate**; the ADC moved it, and the mover was **under-fill (σ = 0.21 LSB: 1.0–1.6 bits of over-claim, `eye_open` collapsing from a 6-bit claim to 0.91 realised)**, not clipping (**28 % clipped costs ≤ 0.34 bits**). A corpus that swept gain would have swept a no-op and reported broad coverage of a variable with no effect. A3 is the corrected axis. The clipped levels stay in the ladder anyway — as the row that *confirms* the null result, which is worth keeping precisely because it is counter-intuitive.

**7.2 Every row is tagged by evidence currency, and calibrated rows in unmeasured families are `report`, not `bar`.** ADR-0022's gate is paid only in **analytic** bits (`L_check`, check width, sync excess), which is why P1–P7 can carry hard bars: their passes do not read a calibrated table. P8 (C4FM), P9 (CSS) and **S2 (OOK/ASK)** lean on families where no δ has been measured; a bar on those rows would be a threshold set against an uncalibrated scale — the exact defect ADR-0015 §2.2 flagged and T-547 half-closed. They are **report-only until T-619 extends the measurement** (done for AM/OOK and C4FM; **CSS measured by T-851, docs/21 §11 — no calibrated bar is supported**), at which point the M-12 review may convert them to bars (tightening, which is allowed) — and that conversion is a *scheduled* event, not a discovery.

**7.3 The support axis (A4) is mandatory, not optional.** T-547's §5.2 (`eye_open`'s quantile table is **non-invertible at 4 and 6 bits** — a 6-bit ask realises 2.41) and §5.3 (a table of N windows cannot express more than log₂ N bits; n = 4200 caps at 12.04) are both **support-dependent** properties. A corpus fixed at 112 symbols would inherit T-547's blind spot wholesale and would never see either. Hence 112 / 512 / 4096.

**7.4 One thing T-547 found that the corpus can only flag, not fix.** `snr` and `evm` as the M1 FSK block computes them are **the same statistic** (ρ = −0.989), so summing them as independent bits double-counts. That is an engine defect (T-616), not a fixture defect; no scene can prevent it. What the corpus *can* do is make it visible: N5's mismatched-parameter rows are where correlated metrics inflate together, and the suite reports the per-metric contributions of the top negative result so a double-count shows as two large, equal terms in one row rather than as a single number.

---

## 8. Templates-on vs templates-off

Every scene runs both. **They are reported in two separate columns and are never averaged, combined, or reduced to one pass rate.** Templates-on measures the **library**; templates-off measures the **engine**. A strong library hides a weak search, and a single averaged figure is the mechanism by which it hides. The convergence/collapse of the two columns is also A5's gap symptom (§6.1), so the split is doing two jobs. The S rows are templates-off by construction and are the only rows that measure real synthesis at all.

---

## 9. What exists, what must be built, and the tickets

**Exists today:** `fm_broadcast_rds`, `adsb_squitter`, `pocsag_pagers`, `acars_message`, `fsk_burst_train`, `tone` (CW), `noise_floor_rise`, `injected_floor`, `occupancy_*`, `trunk_*` (C4FM / DMR / NXDN, with bursty NBFM decoys), `lora_ism_burst`, `retune_diversity`; the five `fixtures/hackrf/2026-09-13` captures; `impairments` (blocker, IM3 coefficient, LO ppm, phase noise, IQ imbalance, DC, spurs, **`adc_gain_db`** — which is the fill knob A3 needs and already exists); and the blind harness (`tests/e2e/src/blind.rs`: strip, seal, atime self-test).

**Must be built**, so M-12 is sized honestly:

| Ticket | What | Rows blocked |
|---|---|---|
| **T-621** ⚠ | *(id collision: T-621 on the board is an unrelated, done M2-hardening ticket, so this row has **no owner** — the T-627 manifest marks every OOK cell `UNFILED`)* A generic **OOK/ASK** sweep generator — the half of ADR-0015 §7's "FSK/OOK sweep" that never existed | S2 |
| **T-622** | Check and payload parameterisation on the generic generators: arbitrary CRC width (8/16/24/32), an **arbitrary polynomial not in the RevEng catalogue**, and a **constant-payload** mode | P6, P7, A7, the random-polynomial negatives |
| **T-623** | Out-of-catalogue structure generators: **OFDM with a non-standard CP**, **DSSS**, **16-QAM** | N3 |
| **T-624** | Analog-voice negative scenes: **NBFM voice** and **AM voice** as standalone negatives (today they exist only as decoys inside the trunking scenes) | N2 |
| ~~**T-625**~~ | ~~Record **ADC fill (σ in LSB)**…~~ — **built 2026-09-21**: `Provenance.noise_sigma_lsb`, `hk_model::FillBucket`, `hk_dsp::floor::adc_fill_ci8`, `hkpy.synth.fill` (§4.4) | F, A3, and ADR-0022 §8's counter |
| ~~**T-626**~~ | ~~The **N5 mismatched-hypothesis population**~~ — **built 2026-09-22** as `hkpy.synth.mismatched_hypothesis` (§4.3) | N5 |
| **T-627** | The **coverage manifest and the sealed hold-out** in `acceptance_mauto` — the §6.4 / §4.5 instrumentation, which is what makes the sufficiency argument checkable rather than asserted | §6 as a whole |

**Not new tickets, deliberately.** The n ≈ 60 000 negative run and the A2 constant/tail-slope reporting belong to **T-576**, which already owns `acceptance_mauto::false_confirm_budget`; a note has been added there pointing at §3 rather than filing a duplicate. The non-FSK ladder measurement is **T-619**. The correlated-metric defect is **T-616**. The quantile-table declaration is **T-617**. Calibration-table sizing and the fill conditioning key are **T-618**.

---

## 10. What the suite may and may not claim before T-375

**T-375 — a 50 Ω terminator capture at several gain states — is a user action and is still outstanding.** ADR-0021 §8.4 lists it inside N4, and T-547 §6 named it as the only way to split the real-capture spread.

Until it lands, N4 runs on the 433 MHz quiet window alone, and the suite:

- **may claim:** that on a real front end with a real antenna, over 200 windows, no false label was produced and the max analytic hold-out bits stayed at *x*;
- **may not claim:** that the receiver's own noise produces no structure. A quiet antenna window is not a signal-free input — it carries LO leakage, spurs, IMD and the skirts of whatever was on the air. A negative result there is a result about the *environment plus the receiver*, jointly, and T-547 §6 measured that the joint quantity has a **~3.9-bit spread that cannot be attributed**;
- **must therefore print**, on every run of N4, the sentence that the terminator row is absent and that the receiver-only null is unmeasured. An absence that is printed is a known gap; an absence that is silent is an over-claim, which is the same defect as a UI implying resolution the front end never captured.

---

## 11. The freeze

Frozen as of 2026-09-21, before the engine exists: §2's axes and their levels; §3's rule that the A2 constant tracks `log₂(n)`; §4's scene list with each row marked **bar** or **report**; §5.2's 8 base seeds × 50 draws and the pass-with-a-flag rule; §5.3's over-claim decision rule; §6.2's four degeneracy detectors; §6.3's four marked NOT-ARGUED axes; §7.2's currency rule.

**The M-12 review may tighten any bar and may convert any `report` row to a `bar`. It may not loosen a bar, may not convert a `bar` to a `report`, and may not reduce an n** — the last because §3 shows a reduced n silently changes what the A2 constant means while the assertion text stays identical.
