# ADR-0022 — The false-confirm budget: deriving the confirm-by-decode thresholds instead of guessing them

**Status:** PROVISIONAL (T-548, 2026-09-21, core interface, planning only). No code comes from this ADR; MAUTO is unscheduled until after M3.
**Touches:** C21 bit framing, C22 decode, C27 inventory; `Emitter`, `Decode`, `CandidatePipeline`, `ConfirmPolicy` ([docs/07 §2.11, §2.15, §2.28](../07-data-model.md)).
**Amends:** [ADR-0015](0015-decoder-synthesis-contracts.md) §5.5 and §11.5 (the confirm rule), §1.3 (which look-elsewhere term a confirm reads).
**Reads on:** [ADR-0021](0021-search-trace-and-negative-result.md) §8 (the false-**label** budget and the shuffled-null control) and **T-547** (are the calibrated nulls stable? — a soft dependency, discharged in §9).
**Records:** the user's budget line from [docs/20 §U1](../20-mauto-decision-brief.md).

## Context

ADR-0015 §5.5 confirms a candidate when hold-out evidence is **≥ 64 bits after look-elsewhere**, with **≥ 3 distinct valid frames**, on a check of **width ≥ 16** (or BCH ≥ 10 parity bits over ≥ 8 codewords). §7 calls every threshold in the ADR an "initial guess, **unverified**". §11.5 and `change_emitter_lifecycle` add the fact that makes a guess unacceptable here: **no rule demotes.** A wrong auto-confirm is undone only by a user deleting the entry.

The blast radius is wider than one wrong row. A Confirmed emitter is the product's claim that a real emitter was *measured*, and CLAUDE.md's rule is that the known-signal database never overrides what was measured — so a false confirm manufactures a measurement-backed fact that outranks the database. It carries its own presence track, it seeds a C18 signature proposal and a C15 rank-1 classification row (§4.2's feedback path), and via §4.3 it can be **saved as a template** that then biases every later search. A guessed threshold on a one-way door with that reach is the wrong shape of decision.

Three things are wrong with the constants as written, and they are different kinds of wrong:

1. **64 bits** is a threshold on `evidence_bits`, a sum that mixes analytic nulls (check width × frames, sync excess, field fit) with **calibrated** ones (bimodality, eye openness, EVM, SNR) that §2.2 itself marks unverified across 8-bit quantisation and gain state. 64 can therefore be reached with 40 bits whose scale nobody has measured. The number is not too high or too low; it is a threshold on the wrong quantity.
2. **3 frames** is a constant standing in for a quantity that depends on the check width and on how many hypotheses were tried. It happens to be right for a searched CRC-16 and wrong everywhere else — it blocks a single template-fixed CRC-24 squitter, which is *stronger* evidence than three frames under a searched polynomial.
3. **Width ≥ 16** is the one that genuinely cannot be derived from a false-confirm budget, and this ADR says so rather than dressing an assumption as arithmetic.

## Decision summary

| | |
|---|---|
| Budget | **At most one wrong Confirmed emitter per week of unattended running**, device-wide (docs/20 §U1). Units: false confirms per device-week, at an assumed and **monitored** **20 000 confirm decisions per device-week**. |
| What may pay | Only **analytic-null** hold-out bits, each net of the look-elsewhere charged **at its own stage for the winning prefix** — never the job-total `look_elsewhere_bits`, never a calibrated-null metric (§2). |
| The one constant | **`min_analytic_holdout_bits` = 24**, = 14.3 bits of budget + 9.7 bits of stated model-error margin (§3). Replaces the 64. |
| Frames | **Not a constant.** `min_differences = max(1, ceil((24 + L_check) / width))`, over `differences` (chance-corrected, FEC-corrected frames excluded per T-210), not `distinct_valid`. Gives 1 for a template-fixed CRC-24, 3 for a searched CRC-16 — the legacy number, for the one case it fitted (§4.2). |
| Width | **Measured by T-577: 16 with the shipped `differences` count, and 8 only with the count change of §4.3.1.** Never below 8. The floor guards degenerate nulls the budget cannot see. With the shipped count, a no-code burst framed with shifts confirms at 2⁻ʷ per decision, and 16 is the smallest width inside the budget. Two further holes are closed by the count, not by any floor (§4.3.1, [results](../results/T-577.md)). A **hard floor of 16 of the 24 bits must come from a check stage**, so sync excess alone never confirms (§4.3). |
| Template-fixed vs searched | The discount for a *found* rather than *specified* check is exactly `L_check`, inside the same inequality — **no separate penalty**. "Template-fixed" means `builtin` or user-authored; a template **discovered by a previous search inherits that search's look-elsewhere** and is treated as searched (§5). |
| Attended vs unattended | **One threshold.** The arithmetic says attended operation buys ~7 bits of headroom; that is a reason it is safer, not licence to lower it (§7). |
| Cross-job multiplicity | Priced **once**, in the budget's denominator. No running per-session counter — a threshold that drifts with uptime would make the same physical evidence worth less on a device that has been on longer. The denominator is instead **monitored at runtime** and the budget claim is void above it (§8). |
| Relation to ADR-0021 | A false **label** is a wrong verdict shown; a false **confirm** is a wrong permanent row. Confirms ⊂ labels. At 20 000 decisions/week the 1-per-1000-jobs label budget alone permits ~7 false labels a week, so it leaves **no** headroom for the confirm budget — which is why this is derived independently and why the confirm gate is always the stricter of the two (§1.3). |
| Tested by | `acceptance_mauto::false_confirm_budget`: **zero** confirms over ADR-0021's negative populations (weak), plus the assertion that actually has power — the **maximum analytic hold-out bits reached on input with nothing to find** stays ≤ 18, measuring the margin directly — plus a **recall control**, because a false-confirm suite without one is passed by never confirming (§10). |
| What T-547 changes | **Nothing in this threshold's soundness**, by construction. A NO-GO leaves 24 and the formula untouched. A GO buys *recall*, through an explicit amendment, and never below the hard check floor (§9). |

---

## 1. The budget, in units the user can judge

### 1.1 The statement

> **Budget.** At most **one wrong Confirmed emitter per week of unattended running**, for one device.

This is the user's number ([docs/20 §U1](../20-mauto-decision-brief.md)), stated as a rate rather than a probability on purpose: a rate is a thing a person can hold an opinion about ("one a week is tolerable, one a day is not"), and a per-decision probability is not. Everything below converts that rate into bits; nothing below changes it.

*Unattended* is the binding case because it is where nobody is looking when the wrong row appears. §7 handles the attended case.

### 1.2 The denominator: how many confirm decisions a week actually contains

A rate over a week only becomes a threshold once you say how many chances the week contains. A **confirm decision** is one evaluation of `ConfirmPolicy.synthesized` against a rank-1 hold-out result for a candidate emitter — not a frame, not a job, and not a beam node.

| Source of confirm decisions | per day | per week |
|---|---|---|
| Auto-analyze jobs reaching `Validate` (ADR-0021 §8.3's stated 10²–10³ jobs/day, assuming every job reaches the gate) | ≤ 10³ | ≤ 7 × 10³ |
| Template-fixed burst confirms — one per newly-seen emitter per window, in a busy ADS-B or 902–928 MHz window | ≤ 10³ | ≤ 7 × 10³ |
| User-triggered analyze | — (unattended) | — |
| **Assumed total, rounded up** | | **N = 2 × 10⁴** |

`N = 20 000` is an **assumption**, rounded upward because the arithmetic is safer that way, and it is the one assumption in this ADR the running device can check on itself: §8 makes it a counter, not a belief.

**Budget term.** One false confirm per N decisions is a per-decision false-confirm probability of `1/N = 5 × 10⁻⁵`, which in bits is

```
B = log2(N) = log2(20 000) = 14.29 bits
```

### 1.3 False labels and false confirms are different events

ADR-0021 §8.3 sets a **false-label** budget of ≤ 1 per 1 000 analyze jobs and says the stricter of the two binds. The two must be put on one scale before "stricter" means anything.

- A **label** is a verdict ≥ `framed` reported to the user. It is revisable: the next look can say something else, and nothing in the inventory is permanent because of it.
- A **confirm** is a lifecycle change. It is not revisable (§11.5: no rule demotes).
- **Every false confirm is preceded by a false label** — confirmation requires a hold-out result at `checked`/`solved`, which is ≥ `framed`. So false confirms are a *subset* of false labels, and the confirm gate's job is to reject nearly all of them.

On one scale: at the same job rate, 1 false label per 1 000 jobs over 7 × 10³ jobs a week is **about seven false labels a week**. The confirm budget is **one wrong row a week**. So the label budget, met exactly, leaves the confirm budget no headroom at all — it would be satisfied only if fewer than one in seven false labels converted to a confirm.

That is the reason this ADR derives the confirm gate from its own budget rather than inheriting ADR-0021's. The resolution of ADR-0021's "the stricter binds" is therefore fixed and not open: **the confirm gate binds, always**, because 5 × 10⁻⁵ per decision is stricter than 10⁻³ per job under any plausible mapping between the two. ADR-0021 may not loosen it, and this ADR does not loosen ADR-0021's.

---

## 2. What may pay for a confirmation

### 2.1 The rule

> **Only analytic-null bits pay for a confirm**, and each is charged the look-elsewhere of **its own stage on the winning prefix**.

```
analytic_holdout_bits = Σ over analytic stages j on the rank-1 prefix, measured on hold-out:
                        ( b_j − L_j )
```

**Analytic** means ADR-0015 §2.2's first list, where the null is a closed-form tail and not a shipped quantile table:

| Metric | Null | Contributes |
|---|---|---|
| `check_distinct_valid` | `width × differences`, with `assist::codes`' posterior and chance-factor handling | yes |
| `sync_excess` | Chernoff/Poisson tail minus pattern width (assist `significance_bits`) | yes |
| `field_fit`, `identity_recurrence` | binomial against random frames | yes |
| `snr`, `bimodality`, `offset_ratio`, `pilot_lock`, `eye_open`, `timing_var`, `evm`, `line_violations`, `bit_structure` | calibrated quantile tables, `synth/calibration/<block>.json` | **no** |

The calibrated metrics keep every other job they have: they rank, they prune against `floor_j`, they order the beam, they are reported in the ladder and in the UI. They do not pay for an irreversible transition.

### 2.2 Why — and why this is not a workaround for T-547

ADR-0015 §2.2 states plainly that whether the calibrated tails hold across 8-bit quantisation and gain state is **unverified**, and T-547 exists to measure it. Thresholding a sum that contains them means the confirm rate depends on an unmeasured quantity — and on a one-way door, "we will find out in M-2" is not a design.

Splitting the sum is not a hedge against T-547 reporting badly; it is the right structure either way. The three analytic nulls are the ones whose scale is a *theorem* about the data, not a property of the front end. A CRC of width *w* is passed by a random frame with probability 2⁻ʷ whether the LNA is at 0 dB or 40 dB, whether the ADC clipped, and whether the block version changed. That invariance is exactly what an irreversible decision should rest on. Meanwhile the product loses nothing it can name: a "decode" that carried no check and no sync is not a decode.

### 2.3 Which look-elsewhere term — the correction must be attributable

ADR-0015 §5.5 says "≥ 64 bits **after look-elsewhere**" without saying *which* look-elsewhere, and ADR-0021 §7A.2 serves a job-total `coverage.look_elsewhere_bits` (18 422 hypotheses → 14.2 bits in its worked example). Those are not the same number, and using the job total here would be wrong twice:

1. **It double-charges.** Each `b_j` in §1.3's ladder is already net of `L_j`. Subtracting the job total again charges the same multiplicity twice for the stages that paid it.
2. **It makes a confirmation depend on unrelated searching.** Under a job total, analysing a busy window — where the engine spends thousands of evaluations on *other* families — would stop an ADS-B squitter confirming, although not one of those evaluations was a chance for *this* hypothesis to fit. The look-elsewhere correction prices the trials that could have produced *this* fit. Trials that could not, do not count.

**So:** the confirm gate reads `analytic_holdout_bits`, a new field computed per-stage on the winning prefix. ADR-0021's `coverage.look_elsewhere_bits` stays what it is — a **reporting** field describing the size of the search — and is never an input to `ConfirmPolicy`. This is the connection ADR-0021 §8.1 asked for, and the answer is that the two fields have different jobs: one describes the search, the other prices a hypothesis.

---

## 3. The derivation

### 3.1 The two terms

```
min_analytic_holdout_bits  =  B  +  M

B = log2(N)                 = 14.29 bits   the budget (§1.2): 1 wrong row per 20 000 decisions
M = the model-error margin  =  9.7 bits    stated below, and measured by §10.2
                            -------------
                            = 24.0 bits
```

### 3.2 The margin, and what it is for

`B` alone would be the answer if the analytic nulls were exact. They are not exact; they are *closed-form under assumptions*, and the margin prices the assumptions:

| Assumption | How it fails | Direction |
|---|---|---|
| **Frames are independent trials** of a 2⁻ʷ event | Frames from one emitter share structure; a constant or slowly-counting payload region makes "distinct" frames near-duplicate trials. `assist::codes` already guards the worst of it — `differences` excludes differences that repeat with a period ≤ 16 bits — but the guard is a heuristic, not a proof | optimistic |
| **The generator found is the generator used** | Every divisor of a fitting generator fits; `assist::codes` prices this as a chance-factor confidence of ≈ 0.1 at 3 frames, 0.4 at 4, 0.95 at 8 (docs/api.md). Below ~8 frames the posterior is genuinely diffuse | optimistic (for identity; see §4.3) |
| **`L_j` counts effectively independent trials** | A beam full of near-duplicate symbol rates is charged as independent (conservative); a proposal operator returning correlated candidates is charged as if independent when it should be charged more (optimistic). ADR-0021 §8.1 item 3 | both |
| **N = 20 000 is the decision rate** | §1.2 is an estimate over a workload nobody has run yet | unknown |

`M = 9.7 bits` is a **factor of 832**: the derivation holds as long as the analytic nulls, taken together, are not optimistic by more than ~830×. It is a **stated judgement**, in the ADR-0015 §7 tradition, and §10.2 gives the measurement that replaces it — the one number in this ADR that a test can directly refute.

### 3.3 What 24 bits does and does not predict

At 24 bits the *nominal* per-decision false-confirm probability is `2⁻²⁴ = 6.0 × 10⁻⁸`, which at 20 000 decisions a week is **one wrong row per ~840 weeks, about 16 years**.

**That number must not be quoted.** It is what the model says, and the whole of `M` exists because the model is known to be optimistic by an unknown amount. The honest statement is:

> The threshold is set so that the budget of one wrong Confirmed emitter per unattended week is met **provided the analytic nulls are not optimistic by more than about 830×**. The realised rate is set by that model error, not by 2⁻²⁴, and no amount of arithmetic reveals it. Only §10.2's measurement does.

This is the same discipline as ADR-0021 §8.4 ("0 of 800 bounds the rate only to ≈ 0.0037") and docs/17 §1's four-decimal error: state what the number rests on, in the same breath as the number.

---

## 4. The three constants, replaced

### 4.1 Bits: 24 analytic hold-out bits (was 64 `evidence_bits`)

```rust
min_analytic_holdout_bits: f32 = 24.0
```

It is lower than 64 and **strictly harder to reach**, because it may be paid only in the currency of §2.1. A result with 70 `evidence_bits` of which 22 are analytic does not confirm under this ADR and did under the old rule; a single template-fixed CRC-24 frame carries 24 analytic bits and confirms under this ADR and did not under the old one. Both changes are the point.

### 4.2 Frames: a formula, not a constant (was ≥ 3)

The gate is the inequality in §4.1. The frame count is what that inequality *implies* for a check-only accounting, and it depends on the width and the multiplicity:

```
min_differences = max(1, ceil( (min_analytic_holdout_bits + L_check) / width ))
```

with two corrections to what is being counted:

- **`differences`, not `distinct_valid`.** `assist::codes` already computes the chance-corrected count: frames counted once however often they repeat, and differences that repeat with a period ≤ 16 bits (constant or alternating payload) not counted. `distinct_valid` over-credits an emitter that sends the same payload repeatedly — which is most beacons. This is the second real defect in §5.5, after the currency.
- **FEC-corrected frames never count** (T-210, unchanged): `differences` is computed over frames valid *without* correction; corrected groups are `corrected_excluded`, worth 0 bits, display only.

Worked, with `L_check` as stated (the generator space plus the `start_bit` / `tail_bits` / `bit_order` / `classes` slots — call the slot product ~5 bits):

| Check | `L_check` | `min_differences` | Legacy rule said |
|---|---|---|---|
| CRC-24, template-fixed (ADS-B) | 0 | **1** | 3 — blocked the case the product exists for |
| CRC-16, template-fixed | 0 | **2** | 3 |
| CRC-8, template-fixed (much of 902–928 MHz) | 0 | **3** | refused outright (width < 16) |
| CRC-16, searched | ≈ 16 + 5 = 21 | **3** | 3 ✓ |
| CRC-8, searched | ≈ 8 + 5 = 13 | **5** | refused outright |
| CRC-32, searched | ≈ 32 + 5 = 37 | **2** | 3 |
| BCH(31,21), 10 parity, template-fixed | 0 | **3 codewords** | 8 codewords |
| BCH, 10 parity, searched | ≈ 15 | **4 codewords** | 8 codewords |

So "3 frames" was correct for exactly one row of that table and was being applied to all of them.

**The single-burst case, honestly.** docs/20 §U1's recommendation — one clean burst confirms when the check was template-fixed and ≥ 24 bits — falls out of the inequality rather than being a special case, but it is *conditional* and the condition should be stated rather than hidden. Trying a fixed template at several candidate framings in the window is itself a multiplicity: with `tested` framings and one valid, the check contributes `width − log2(tested)` bits, so a single CRC-24 squitter reaches 24 bits only when the preamble stage repays that log. It does, easily, in the burst path (§6 hands the engine a burst-gated window, `tested` is a handful, and a matched 8-pulse preamble carries its own `sync_excess` bits) — but the policy does **not** contain a "one frame confirms" branch. It contains one inequality, and the squitter satisfies it. There is no predicate to get wrong, and no case where a template silently exempts a hypothesis from its own multiplicity.

### 4.3 Width: a floor of 8, and an admission (was ≥ 16)

Two conditions replace the flat `width ≥ 16`:

```rust
min_check_width: u16 = 16         // T-577, measured; 8 only with §4.3.1's count
hard_check_floor_bits: f32 = 16.0 // at least this much of the 24 comes from a check stage
```

**The floor of 8 is not derived from the budget, and this ADR will not pretend otherwise.** The budget constrains a *rate*, and §4.2's inequality already converts any width into the frame count that meets it — arithmetically, width 4 with 7 differences is the same 28 bits as width 16 with 2. What the budget cannot see is a **degenerate null**: below about a byte, a check can be satisfied by a framing artefact rather than by a code, the `differences` guard has too little to work with, and the per-frame independence assumption (§3.2) degrades fastest. 8 was a judgement that a byte is the smallest unit where the accounting still means something. T-577 measured it against ADR-0021's N2/N3 populations and three synthetic framing artefacts, and the number moved **up**; see §4.3.1.

#### 4.3.1 Measured (T-577)

Source: [docs/results/T-577.md](../results/T-577.md), harness `hk-estimate/tests/degenerate_null.rs`. N2/N3 alone never reach the gate at any width (0 of 4000 per cell), with one exception, (A) below. Three degenerate nulls do reach it, and each has a mechanism, not only a rate:

- **(A) Init-cancel, independent of width.** An affine check with init = all ones is satisfied by `1^w ‖ 0…0`: the first *w* bits cancel the register and the rest is idle fill. With xorout ≠ 0 the frame is `init ‖ 0…0 ‖ xorout`. Neither frame is periodic as a whole, so the short-period guard counts it. A CW or voice slicer emits it: **2.0–2.3 × 10⁻² per decision at w = 32 on N2**, where one frame alone clears the gate. No floor fixes this.
- **(B) Shift: the floor for the shipped count.** For a linear check, g ∣ P implies g ∣ x^k·P. When a framer offers zero-padded shifts of one burst (onset jitter, a block grid, a search over offsets), every shift is valid, and each is a distinct "difference". **One 2⁻ʷ event then clears the gate at every width ≥ 8** (seeded rate ≈ 1.0). The realised rate is 2⁻ʷ per decision: 3.9 × 10⁻³ at w = 8 (78× the budget), 2.4 × 10⁻⁴ at 12 (4.9×), and **1.5 × 10⁻⁵ at 16 (0.31×, inside, with 1.7 of the 9.7 margin bits left)**. A GF(2) rank cap does not remove it, because shifted copies are linearly independent. Searched checks have it at **every** width: 1–15 % of no-code beacon windows, and the §8.2-style null control is not built to see it.
- **(C) Period: the reason for "never below a byte".** A payload repeated at lag ord(x mod g) satisfies the check whatever the payload. ord ≤ 2^w − 1, so at w = 4 a 15-bit code sent twice passes **every frame, with certainty**, and no count can remove it.

**The count change.** A valid frame adds a trial only if two things hold:

1. It is not degenerate: it is not short-periodic even with up to *w* bits trimmed from either end. This closes (A).
2. Its zero-trimmed polynomial is not a multiple (or divisor) of one already counted, because m·P is valid by construction once P is. This closes (B), including frames that hold two copies of one burst.

The count is then capped at the GF(2) **affine rank + 1** of the valid frames. That is where independence actually saturates: at the payload's varying-bit dimension (a slow sensor ≈ 7, a counter log₂k + 2, a squitter 41), not at a frame count. With this count, every N2/N3 and artefact cell reads 0 of 4000 at w = 8, except (C) at w = 4. It costs the five real-emitter models nothing: it equals `differences` at every k.

**So:** `min_check_width = 16` while `differences` is the `CheckTally` count. It returns to **8** only when the count above ships, re-measured by the same harness. It never goes below 8. (A) and the searched half of (B) need the count change **whatever the floor**, and T-575 carries both. `distinct_valid` versus `differences` (T-575's swap) measured a gap of **0** on every real emitter. The swap's value is on the null side: the short-period guard takes N2 at w = 24/32 from 44–48 % to 0.

**The hard check floor is derived**, from what a confirmation claims. Confirm-by-decode says *this was decoded*. Sync excess has the weakest independence assumptions of the three analytic nulls (a periodic signal repeats its own patterns), and a result carrying 24 bits of sync excess and no check has not decoded anything. So at least 16 of the 24 bits must come from a `check_distinct_valid` contribution. Lowering the width floor to 8 without this would let a sync-only result through the door the width floor was informally holding shut.

**What the chance-factor confidence does *not* gate.** `assist::codes`' confidence (0.1 at 3 frames, 0.95 at 8) prices *which generator* was found, not *whether the frames check out*. A chance multiple of the true generator still means real structure with a real check: the emitter is real, the *identity* is uncertain. So it gates the **label**, under ADR-0021's budget, and not the confirm. This is the same separation ADR-0021 §7A.5 already makes — a Confirmed emitter with no identity at all is a legal, intended state — and it is why a `structured-unidentified` result may confirm.

---

## 5. Template-fixed versus searched

### 5.1 What "template-fixed" means, and the laundering rule

A check is **template-fixed** when the generator, width, start bit, tail, bit order and class count were all specified before the data was seen, by a template whose `provenance` is `builtin` (shipped with the engine) or `user-authored`. Then `L_check = 0`, because zero hypotheses were tried.

> **A template discovered by a previous search is not template-fixed.** §4.3's save-as-template path records, at save time, the look-elsewhere the discovering search spent (`discovery_look_elsewhere_bits`), and every later use of that template **inherits** it as `L_check`.

Without this rule a search can launder its own multiplicity: try 10⁴ polynomials, find one, save it as a template, and every subsequent confirmation gets it for free — and the ticket's own blast-radius argument (a false confirm becomes a template that biases later searches) closes into a loop that reinforces itself. Inheriting the L breaks the loop at the only point where the information still exists.

### 5.2 The searched-generator discount is `L_check`, and nothing else

ADR-0021 §7A.5 consequence 3 records that a found CRC-16 is weaker evidence than a specified one and assigns the number to this ADR. The number is:

> **The discount is exactly `L_check`, charged inside the §4.1 inequality. There is no separate penalty, no multiplier and no profile-dependent surcharge.**

`assist::codes` already computes it — `evidence_bits = width × differences − log2(hypotheses)` (docs/api.md) — so the engine is not asked for a new quantity, only to stop discarding it at the confirm gate. Adding a second penalty on top would be charging the same multiplicity twice, which is §2.3's error in a different costume.

The consequence for ADR-0021's interim rule: its holding pattern ("until T-548 lands, a `structured-unidentified` result with a searched check confirms only at `deep` with the null control passed") is now **replaced** by the inequality plus §5.3. A searched check confirms at `standard` as well as `deep`, provided it clears 24 analytic bits with `L_check` charged *and* the null control passed. `quick` still never confirms (ADR-0021 §8.2: K = 0).

### 5.3 The shuffled-null control stays mandatory for open searches

ADR-0021 §8.2's control — re-run the winning prefix unchanged over time-reversed and phase-randomised surrogates, cap the verdict if `holdout_bits − b_null < 8` — is **retained unchanged and unweakened**, and this ADR does not re-open its numbers. Its relationship to the arithmetic above is worth stating once, because they look like two guards against the same thing and are not:

- `L_check` is the **analytic** estimate of how many chances this hypothesis had. It is exact about a space it may have mis-counted.
- The null control is the **empirical** measurement of this search's propensity to find structure in structureless data on *this front end, at this gain state, at this quantisation*. It prices the failure §3.2's table calls "both directions".

They disagree exactly where the analytic count is wrong, and the control can only **cap**. Where they disagree, the measurement wins by capping. That is the right asymmetry on a one-way door.

---

## 6. What `ConfirmPolicy` therefore holds

```rust
// hk_pipeline::inventory::ConfirmPolicy::synthesized — actor `hk-pipeline/confirm-synth@2`
pub struct SynthesizedConfirm {
    /// §3: 14.3 bits of budget + 9.7 bits of stated model margin.
    min_analytic_holdout_bits: f32,   // 24.0
    /// §4.3: at least this much of the above from a check stage. Derived.
    hard_check_floor_bits: f32,       // 16.0
    /// §4.3: a degenerate-null floor. MEASURED by T-577 (§4.3.1): 16 with the CheckTally
    /// count, 8 only with §4.3.1's count change; never below 8.
    min_check_width: u16,             // 16
    /// §1.2: the budget's denominator. Monitored, not trusted (§8).
    assumed_decisions_per_week: u32,  // 20_000
    /// §5.3: ADR-0021 §8.2's control must have run and passed for an open search.
    require_null_control_when_searched: bool, // true
    /// Unchanged from ADR-0015 §5.5 condition 4.
    max_suspect_detection_fraction: f32,      // 0.5
    forbid_overload_in_window: bool,          // true
}
```

The gate, in order:

1. `result.check.width >= min_check_width` (16; 8 with §4.3.1's count);
2. `check_bits >= hard_check_floor_bits`, where `check_bits = width × differences − L_check` over hold-out, `differences` per §4.2 and T-210;
3. `analytic_holdout_bits >= min_analytic_holdout_bits`, summed per §2.1 with per-stage `L_j`;
4. if the winning check was **searched** (including an inherited-L discovered template, §5.1): the ADR-0021 §8.2 null control ran and did not cap;
5. front-end trust: ≤ 50 % suspect detections and no overload in the window (unchanged);
6. profile is not `quick` (unchanged).

**Not stored:** `min_evidence_bits` (deleted — wrong currency), `min_distinct_valid` (deleted — derived per job by §4.2), the legacy flat `min_check_width = 16` (replaced by the hard check floor plus a measured width floor: 16 with the shipped count, 8 with §4.3.1's count — T-577).

**Unchanged:** the rule never demotes; a user delete wins; a partial verdict never changes lifecycle state; one promoted row per `output_kind`; the lifecycle reason stays backend-rendered and now names the arithmetic — e.g. *"decoded by synthesized pipeline `generic-fsk-framed`: CRC-16, 3 differing frames valid on hold-out without correction, 48 − 21 = 27 analytic bits against a 24-bit threshold; null control passed with an 11.4-bit margin."* A user reading that can tell which number was close.

---

## 7. Attended and unattended need not share a threshold — and do

The ticket asks whether an unattended auto-confirm and a user-triggered analyze confirm should differ. The arithmetic first: a user-triggered confirm decision happens because a person pressed Analyze, so its weekly multiplicity is 10¹–10², not 2 × 10⁴. That is `log2(20 000 / 100) ≈ 7.6` bits of slack — a user-triggered gate could sit near 17 bits and meet the same one-a-week budget.

**It should not.** One threshold, for three reasons:

1. **The blast radius is identical.** The row is equally permanent, equally a measurement-backed fact outranking the database, and equally a seed for a C18 signature and a C15 classification row. Nothing about the user having pressed a button makes the wrong row less wrong.
2. **Attended is not supervised.** A user who starts an analyze on a busy band is not reading every confirmation it produces; §11.5's "only a user deletes an entry" assumes they noticed.
3. **A second constant is a second thing to get wrong**, on the interface CLAUDE.md flags as `core_interface`.

The 7.6 bits are therefore **headroom, not budget**: the attended path runs at a false-confirm rate roughly 200× inside its own budget, which is a property to report and not to spend. The one asymmetry that stands is the existing one — `quick` never confirms.

---

## 8. Per-job, per-session, per-week: where the multiplicity is priced

ADR-0021 §8.1's first objection — a per-job look-elsewhere cannot bound a per-session rate, since a hundred jobs on noise are a hundred chances — applies verbatim to confirmations, and this ADR answers it **by construction rather than by a new term**:

> The cross-job multiplicity is priced **once**, as `B = log2(N)` in §3.1. It is a constant in the threshold, not a counter in the engine.

The rejected alternative is a running per-session or per-emitter correction that grows with the number of decisions the device has made. It fails on its own terms: it would make the *same physical evidence* — the same squitter, the same CRC, the same SNR — insufficient on a device that has been running a month and sufficient on one booted an hour ago. Evidence in a signal does not depend on the receiver's uptime. What depends on uptime is the *rate*, and a rate is what the budget states.

But a fixed threshold delivers the budgeted rate **only at the assumed decision rate**, so the assumption becomes a runtime obligation:

- The device counts `ConfirmPolicy.synthesized` evaluations over a rolling 7 days.
- If the count exceeds `assumed_decisions_per_week` (20 000), **the budget claim is void** and the device says so — a surfaced condition, not a silent one, in the register of ADR-0021's "the null control ran and passed" being a visible fact rather than an absence.
- The counter is also the evidence that revises `N`: after a month of real unattended running, §1.2's table stops being an estimate.

This costs one counter and one surfaced state, and it converts the derivation's weakest input into something checkable on the device that depends on it.

---

## 9. What changes when T-547 lands — and what does not

**T-547 has landed** ([docs/21](../21-evidence-bits-under-quantisation.md), 2026-09-21): **CONDITIONAL**, conditioning key **ADC fill** (not gain, not clip fraction), **δ = 1.8 bits at a claimed 6 bits** and 3.3 at 8, plus a separate ~2-bit unattributed spread on real captures. It deliberately did **not** write the amendment below; §8 of that note says what the amendment should say if it is written.

T-547 measures whether the calibrated nulls (bimodality, eye openness, EVM, SNR and the rest of §2.1's second list) have stable tails across gain state, clipping fraction and 8-bit quantisation, and returns GO / CONDITIONAL / NO-GO.

**What does not change, under any of the three outcomes:**

- `min_analytic_holdout_bits = 24`, `hard_check_floor_bits = 16`, `min_check_width = 8`, and §4.2's formula. None of them reads a calibrated metric.
- The margin `M = 9.7 bits`. It prices the *analytic* nulls' assumptions (§3.2), which T-547 does not measure. §10.2 moves `M`; T-547 does not.
- The soundness of the budget argument. This is the point of §2: the derivation was built so that a NO-GO is survivable without a redesign.

**What each outcome changes:**

| T-547 reports | Effect here |
|---|---|
| **NO-GO** — the tails move with gain state or clipping | **Nothing.** The calibrated metrics were already ranking-only for confirm purposes. The damage lands on ADR-0015 §1.3's `floor_j` values and 12-bit caps, on search order, and on whether `evidence_bits` may be shown to a user as a significance at all — all outside this gate. |
| **CONDITIONAL** — stable once conditioned on a named key (gain, clip fraction, measured SNR) | Still nothing automatic. An **explicit amendment to this ADR** may then admit conditioned calibrated bits toward the 24 at a discount of the measured spread δ — buying **recall**, never validity — and never below `hard_check_floor_bits`: calibrated bits may top up a check, never replace one. |
| **GO** — one table serves every gain state, spread δ bits | The same amendment, with a smaller δ. The expected gain is recall on weak-but-real signals whose check is narrow, which is the population §4.3's floor is hardest on. |

**The rule for that amendment, stated now so it is not decided under pressure later:** calibrated bits may be admitted only at a discount of the **measured** spread δ, only above the hard check floor, and only with §10's suite re-run — because admitting them raises the realised false-confirm rate, and the only evidence about by how much is the measurement in §10.2.

So the dependency is real but bounded: **T-547 does not gate this ADR, and T-547's answer cannot invalidate it.** The numbers stated here are derived under assumptions named in §3.2 and §4.3, not measured, and they are labelled as such everywhere they appear.

---

## 10. How it is tested

> **The fixture corpus A1/A2/A3 run on is specified in [docs/22](../22-mauto-acceptance-corpus.md)** (T-551, frozen 2026-09-21). One thing there is load-bearing for §10.1: a sample of `n` negative decisions resolves a tail no finer than `log2(n)` bits, so **A2's `18.0` is a function of `n`** — at n = 1600 it detects optimism above 7.4 bits (conservative against §3's 9.7-bit margin), and at a smaller n the same constant would read green with no power at all. docs/22 §3 freezes `n` alongside the constant. It amends nothing here.

Protocol is ADR-0021 §8.4's, unchanged and non-negotiable: fixtures replay **through the mock SDR** behind the ordinary device interface, jobs start via `POST /api/analyze`, targets come from **blind detection** or an ad-hoc band and **never** from a truth frequency, truth is loaded only by the assert harness, and jobs are bounded by `max_evaluations` rather than by wall so the suite is deterministic.

### 10.1 Three assertions, because one of them has no power and one of them is passed by doing nothing

**A1 — the direct assertion (necessary, weak).** Over ADR-0021 §8.4's negative populations N1 (thermal noise, 400), N2 (energy without symbols, 200) and N4 (real empty capture), at each of `standard` and `deep`:

```
confirms == 0        and        emitters_created == 0
```

**A2 — the assertion with power.** The same runs report the **maximum `analytic_holdout_bits` reached by any rank-1 hold-out result on input with nothing to find**, and its distribution:

```
assert max_analytic_holdout_bits_on_negatives <= 18.0        // 6 bits below the threshold
report p50, p99, max, per population and per profile
```

This is the measurement that tests §3.2's margin **directly**, instead of waiting for a threshold crossing that a 24-bit gate makes vanishingly rare. It is the sharpened form of T-568's "early-warning number": the tail of the analytic null on real negative input *is* the quantity `M` was invented to cover. If the observed maximum is 21 bits, the true margin is 3 bits, not 9.7, and the budget is missed by a factor of ~100 while A1 still reads zero.

**A3 — the recall control, because a false-confirm suite with no recall control is passed by never confirming.** ADR-0015 §7's positive rows must still confirm under the new gate, and two of them are specifically about this ADR's changes:

| Case | Must |
|---|---|
| ADS-B squitter scene, burst path | ≥ 14/16 single squitters `solved`; the burst set confirms; **and at least one single squitter confirms on one frame** (the §4.2 case the old rule blocked) |
| A CRC-8 emitter in 902–928 MHz, template-fixed | confirms at 3 differing frames (the case the old width floor refused outright) |
| RDS on `fm_100p8M`, POCSAG, ACARS | confirm, as §7 already requires |
| A repeated-payload beacon (same bytes every frame) | **does not** confirm on repeat count alone — `differences`, not `distinct_valid` (§4.2) |

### 10.2 What the suite honestly bounds, and what it would take to bound the budget

Following ADR-0021 §8.4's precedent exactly:

- A1 at n = 800 per profile with zero confirms bounds the true per-decision false-confirm rate to **≤ 3.7 × 10⁻³ at 95 % confidence** (the rule of three). The budget is **5 × 10⁻⁵**. So **A1 cannot demonstrate the budget and must not be quoted as doing so**: it is 75× too weak. It can only fail to contradict it.
- Demonstrating 5 × 10⁻⁵ from a zero-failure run at 95 % confidence needs **n ≈ 60 000** negative decisions. That is a real number and it is not obviously out of reach for a batch run over synthetic negatives — but it is not in the acceptance suite's budget, and inventing it in CI would be the same defect as quoting it. **T-576** owns the question of whether a nightly or milestone run at that scale is worth building.
- A2 is where the confidence actually comes from. It does not estimate a tiny rate from rare events; it measures the location of a tail directly, which is a quantity 800 samples *can* say something about.

**The failure message must be legible**, so it states the budget, the threshold, the observation and the implied margin rather than a bare inequality:

```
acceptance_mauto::false_confirm_budget  FAILED  (A2)

  budget            1 wrong Confirmed emitter / device-week at 20 000 decisions/week
                    = 5.0e-5 per decision = 14.3 bits          [ADR-0022 §1]
  threshold         24.0 analytic hold-out bits
                    = 14.3 budget + 9.7 assumed model margin   [ADR-0022 §3]
  observed          max analytic hold-out bits on 1 600 negative decisions: 21.4
                    (N2 CW carrier, job 388, deep; p99 16.2, p50 8.1)
                    -> realised margin 2.6 bits, not the assumed 9.7
                    -> the analytic nulls are optimistic by >= 2^7.1 = 137x more
                       than ADR-0022 §3.2 assumed
  implication       at this margin the budget is missed by ~137x:
                    ~2.4 wrong Confirmed emitters per week, not 1 per 840 weeks
  the observation   check CRC-16 searched, width 16, differences 3, L_check 20.4,
                    sync_excess 6.1 bits, null control margin 9.2 bits (did not cap)
                    replay: just replay fixtures/negatives/cw-carrier-388.sigmf-meta
```

A reader of that knows which assumption broke, by how much, and which fixture to replay. A bare `assert!(max <= 18.0)` would tell them none of it.

### 10.3 The runtime check the suite cannot do

§8's counter is the other half: the suite measures the *threshold's* behaviour on negatives, and the counter measures whether the *denominator* the threshold was derived from is the one the device is actually running at. Neither substitutes for the other, and only the pair supports the sentence "one wrong Confirmed emitter per unattended week".

---

## 11. Deltas

### 11.1 ADR-0015

- **§5.5** — conditions 1–3 replaced by §6's gate; `min_evidence_bits` and `min_distinct_valid` deleted; `min_check_width` 16 → 8 with `hard_check_floor_bits`; condition 4 (front-end trust) unchanged; the actor becomes `hk-pipeline/confirm-synth@2`. The "single frames don't auto-confirm" sentence and §10 open question 1 are **settled** by §4.2 + docs/20 §U1.
- **§11.5** — the restated thresholds updated to match; "no rule demotes", the T-210 corrected-frame invariant and the one-promoted-row rule unchanged.
- **§1.3** — a note that `evidence_bits` remains the search-order and result-rank key and is **not** the confirm key; `analytic_holdout_bits` is added beside it.
- One pointer line added in this branch; the full rewrite belongs to **T-575**.

### 11.2 ADR-0021

- **§7A.5 consequence 3** — the interim rule ("until T-548 lands, confirms only at `deep` with the null control passed") is discharged by §5.2: the discount is `L_check`, and a searched check may confirm at `standard` with the control passed.
- **§8.3** — "the stricter binds" resolves to the confirm gate binding, per §1.3. Neither budget loosens the other.
- **§8.2** — retained unchanged. `min_null_margin = 8` stays ADR-0021's number and T-568's to measure.

### 11.3 docs/07

- **§2.28 `ConfirmPolicy`** — the §6 fields; `min_evidence_bits`/`min_distinct_valid` removed.
- **§2.15 `Decode`** — `provenance` gains `analytic_holdout_bits`, `check_bits`, `l_check`, `check_searched` and `template_provenance`, so a confirmation's arithmetic is reconstructible from the stored row rather than only from the job.
- **§2.11 `Emitter`** — the lifecycle reason text carries the arithmetic (§6).
- Template rows gain `discovery_look_elsewhere_bits` (§5.1), set at save-as-template and never null for a discovered template.

### 11.4 Task graph

| Ticket | Owns |
|---|---|
| **T-575** | The derived `ConfirmPolicy` inside ADR-0015 §10's M-9 / §11.9's CP-4: `analytic_holdout_bits` with per-stage `L_j`, `differences` not `distinct_valid`, the §4.2 formula, the template-provenance and inherited-L rule, the §8 decision-rate counter, and the ADR-0015 rewrite this branch only points at. `core_interface`. |
| **T-576** | `acceptance_mauto::false_confirm_budget`: A1, A2 and A3, the legible failure report, and the open question of whether an n ≈ 60 000 negative run is worth building outside CI. |
| **T-577** | Measure the two numbers §4.3 admits are assumptions: the check-width floor and the degenerate-framing null, against ADR-0021's N2/N3 populations. |

---

## Options considered

- **Keep 64 bits and tighten it.** Rejected: the defect is the currency, not the value. Raising a threshold on a sum that contains unverified calibrated bits raises it on an unknown scale, and it would push the single template-fixed squitter further out of reach while doing nothing about a 70-bit result carrying 22 analytic bits.
- **Derive a threshold from the full `evidence_bits`, conditional on T-547.** Rejected: it makes an irreversible transition wait on a measurement, and it makes the threshold's validity a hostage to an outcome nobody controls. §2 costs recall and buys a number that holds whatever T-547 says.
- **A running per-session look-elsewhere term.** Rejected in §8: it makes identical evidence worth less on a device with longer uptime, which is wrong about what evidence is.
- **A separate, lower threshold for user-triggered analyze.** Rejected in §7: the 7.6 bits are real and the blast radius is not smaller. Reported as headroom.
- **A fixed penalty for searched checks** (e.g. "a searched check counts at half width"). Rejected in §5.2: `assist::codes` already computes the exact correction, and a second penalty double-charges the same multiplicity.
- **Confirm only on template-fixed checks; open searches never confirm.** Rejected: it would make the 902–928 MHz playground — the canonical case, where nothing has a template — permanently unconfirmable, and it contradicts ADR-0021 §7A.5's `structured-unidentified` being a first-class result that may confirm.

## Consequences

- The irreversible transition rests on nulls that are theorems about the data rather than tables about the front end, so it survives T-547 in every direction (§9).
- **Recall changes in both directions**, deliberately: a single template-fixed CRC-24 burst and a template-fixed CRC-8 sensor now confirm, and a result whose bits were mostly calibrated no longer does. The second is a real loss of recall on signals with no check, and the answer to those is `structured-unidentified` with an honest `Resolution`, not a confirmation.
- The frame threshold stops being a number and becomes an inequality, which is one fewer constant and one more thing to compute at the gate.
- Two new stored fields on every synthesized `Decode`, one on every discovered template, and one rolling counter.
- The budget is stated in units the user set and can revise: change "one a week" and §3.1 re-derives in one line, `B = log2(N × weeks_per_false_confirm)`.
- **The residual false-confirm rate at the chosen numbers is not known**, and this ADR says so where the number would otherwise go (§3.3). It is bounded by an assumed 9.7-bit margin whose only evidence is §10.2's measurement, which does not exist yet.

## Open questions (for the user)

1. **Is one wrong Confirmed emitter per unattended week still the number?** docs/20 §U1 proposed it and it is recorded here as settled. Everything downstream is one line of arithmetic away from a different answer, so revising it is cheap now and expensive after M-9.
2. **Is a confirmation genuinely irreversible, or is the right fix a reversible confirm?** This ADR takes §11.5's "no rule demotes" as given and pays for it in bits. A system that could demote on contradicting evidence would need far less margin. That is a data-model question, not a threshold question, and it is the one change that would make this ADR mostly unnecessary.
3. **Is an n ≈ 60 000 negative run worth building** outside CI, to bound the budget rather than merely fail to contradict it (§10.2)? It is the only way the headline claim becomes measured rather than derived.

---

*Unverified in this ADR: the 9.7-bit model margin `M`, the 20 000-decisions-per-week denominator `N`, the 16-bit hard check floor (the width floor is measured, §4.3.1 — T-577), the ~5-bit slot product used in §4.2's worked `L_check` values, and the 18-bit ceiling asserted in §10.1's A2. All are first guesses or stated judgements in the ADR-0015 §7 tradition, to be measured by T-576 and T-577 and never loosened after seeing results. The budget itself (§1.1) is the user's decision, not a guess.*
