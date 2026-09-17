# 17 — Short bursts against the open set: the trade, priced across seed bases

**Decision brief for the user (T-364, 2026-09-17). Nothing here has been adopted; no floor has moved.**

T-328 found that the shipped classifier rejects a large fraction of genuine short bursts from their
own class, identified a fix (fitting the class densities over pooled window lengths), and priced it
in a single draw quoted to four decimals. T-428 then established that the number on the other side
of that trade — held-out unknown recall — has a **draw standard deviation of 0.0084**, so the
four-decimal comparison that made the fix look like it "lands exactly on the 0.90 floor" was never a
comparison at all (ADR-0016 §7.1, §7.2).

This document re-measures both sides over **eight seed bases**, reports a mean with a spread at every
point, and says plainly which options are distinguishable from ADR-0016 §7's floor and which are not.
It also finds that the headline 39.4 % understated the gap, for a reason that changes the question.

Everything below is re-derivable with `just t364-curves` (≈20 min per feature set). The harness is
`crates/hk-classify/src/bin/t364-curves.rs`; the raw JSON it writes carries every individual draw.

---

## 1. The answer in one page

**Three findings, then the choice.**

1. **The capability gap is about twice as large as the headline.** T-328's 39.4 % was measured with
   only **C14's symbol-geometry window** shortened — the classifier still saw the full-length
   snippet. A genuinely short emission is short in *both* views, and measured that way the shipped
   classifier rejects **70.3 % ±1.7 %** of a taxonomy class's own N/8 bursts, against 9.4 % ±1.4 %
   of its full windows. (The 39.4 % reading reproduces exactly: 38.4 % ±2.1 % here.)

2. **"Pooled fitting" is two different experiments with wildly different prices**, and only one of
   them was the one priced. Pooling **C14's window** (`pooled-c14`, T-328's protocol) costs
   ~0.06 of held-out unknown recall. Pooling the **snippet and C14's window together**
   (`pooled-both`) — which is what a genuinely short emission actually is — costs ~0.25 and puts
   the open set at 0.707, nowhere near the floor.

3. **T-328's ordering reversal is real, it survives eight draws, and it is decisive.** At a pooled
   fit the four cyclic dimensions hold the open set *better* than the shipped max, and the gap is
   larger than the draw spread: **0.9088 ±0.0053 against 0.8911 ±0.0112**. That is the difference
   between clearing the floor and not clearing it. The four-dimension expansion should be
   re-evaluated in the same breath as pooled fitting, exactly as T-328 said — on this evidence it
   is the *only* configuration that buys the C14-window half of the burst gap while holding the
   floor.

**And the finding nobody asked for, which may matter most:** the shipped classifier's open set is
**already at the floor on genuine short bursts** — 0.8949 ±0.0159, with 5 of 8 draws below 0.90.
The 0.95 the gate reports is a full-window number. On the ephemeral emissions CLAUDE.md makes
first-class, today's open-set honesty is not distinguishable from the floor before anything is
changed.

**What the user is being asked.** Options 1–4 in §6. In short: there is one option that buys a real
piece of the burst gap and stays above the floor (four dimensions + pooled-c14); one that buys the
whole gap and fails the floor decisively (pooled-both); one that changes nothing; and one that
refuses both and puts the effort into making the features length-invariant at source instead, which
is not priced here.

---

## 2. What is being measured, and over which populations

ADR-0016 §7.1 requires every figure to name its population. Three populations appear here.

| Figure | Population |
|---|---|
| **own-class `unknown` rate** | 21 taxonomy classes × gate+{0, 5, 10, 15} dB × 10 dev seeds = **840** snippets, seeds disjoint from every seed any fit here touched. T-328's n. Lower is better: it is a genuine member of a class being refused its own class. |
| **held-out unknown recall** | **the gate's own draw**: the 11 held-out generators × 20/25/30 dB × 12 trials = **396** snippets, built exactly as `m3_grid.rs::held_out` builds it, with the gate's own predicate (`family == unknown \|\| open_set_score ≥ 0.5`). This is ADR-0016 §7 row 4's population — **not** the harness's held-out reading and **not** the six-generator subset (§7.2 rows 2 and 3). |
| **known-side accuracy** | the same 840 snippets: right family, right class. |

A **seed base** moves the evaluation draw and nothing else; the fits always use the shipped
protocol's dev seeds. The eight bases are `ACCEPTANCE_SEED_BASE + 100 000 … + 800 000`, which are
**exactly T-428's eight**, and base 6 is the gate's own seed range.

### Three truncations, because "a short burst" is ambiguous

| Label | The snippet the classifier sees | C14's window | What it models |
|---|---|---|---|
| `full` | N | N | a long emission |
| `burst(C14)` | N | N/8 | C14 got a short look at a long emission — **T-328's and `cyclic_line_window.rs`'s protocol** |
| `burst(both)` | N/8 | N/8 | **a genuinely short emission**, which is what the pipeline hands the classifier for a burst |

### Three fitting protocols

| Arm | Fitted over `(snippet, C14)` truncations | |
|---|---|---|
| `builtin` | — | the shipped `densities-1.json` / `densities-below-gate-1.json`, untouched |
| `full` | `(1,1)` | both models refitted here under the shipped protocol — the control that shows the harness reproduces what ships |
| `pooled-c14` | `(1,1) (1,2) (1,4) (1,8)` | **T-328's pooled fit** |
| `pooled-both` | `(1,1) (2,2) (4,4) (8,8)` | pooled over what a genuinely short emission looks like in every dimension |

Both shipped models are refitted in every arm. Refitting only the claiming model would measure a
hybrid of the old protocol and the new.

### The harness is validated against both prior measurements

- **Against T-428.** The `builtin` arm's eight draws are 377, 379, 380, 377, 378, **370**, 377, 381
  of 396 — the *same multiset* T-428 reported (370, 377, 377, 377, 378, 379, 380, 381), mean 0.9530,
  sd 0.0084 to four decimals. The harness is reading the gate's line, not one of the other two.
- **Against T-328.** `builtin` / `burst(C14)` gives 38.4 % ±2.1 % against T-328's 39.4 %, and with
  the four dimensions on, 57.6 % ±2.5 % against T-328's 56.4 %. Full-window own-class `unknown` is
  9.4 % here against T-328's 11.9 %, which is code that has legitimately moved since (T-311, T-312,
  T-404, T-427 all landed in between, and the gate baseline moved 0.9747 → 0.9520 with them).
- **`full` reproduces `builtin`** to within a snippet on every cell, so any difference a pooled arm
  shows is the protocol and not the harness.

---

## 3. Curve 1 — the shipped feature set (single `cyclic_db`, the max of four line significances)

All cells are **mean ±sd [min, max] over 8 seed bases**.

**A class's own snippet called `unknown`** — lower is better.

| arm | `full` | `burst(C14)` | `burst(both)` |
|---|---|---|---|
| `builtin` | 0.094 ±0.014 [0.070, 0.112] | 0.384 ±0.021 [0.344, 0.412] | **0.703 ±0.017 [0.681, 0.738]** |
| `full` | 0.094 ±0.014 | 0.383 ±0.022 | 0.704 ±0.017 |
| `pooled-c14` | 0.093 ±0.015 | **0.111 ±0.018** | 0.580 ±0.021 |
| `pooled-both` | 0.104 ±0.013 | 0.124 ±0.015 | **0.179 ±0.013** |

**Held-out unknown recall, the gate's draw** — higher is better, floor 0.90.

| arm | `full` | `burst(C14)` | `burst(both)` |
|---|---|---|---|
| `builtin` | **0.953 ±0.008 [0.934, 0.962]** | 0.971 ±0.007 | 0.895 ±0.016 [0.874, 0.919] |
| `full` | 0.953 ±0.008 | 0.972 ±0.008 | 0.895 ±0.016 |
| `pooled-c14` | **0.891 ±0.011 [0.876, 0.914]** | 0.898 ±0.011 | 0.854 ±0.017 |
| `pooled-both` | **0.707 ±0.004 [0.702, 0.712]** | 0.705 ±0.014 | 0.607 ±0.017 |

**Known-side accuracy** — the rest of the price.

| arm | right family, `full` | right class, `full` | right family, `burst(both)` |
|---|---|---|---|
| `builtin` | 0.905 ±0.014 | 0.726 ±0.011 | 0.242 ±0.020 |
| `pooled-c14` | 0.906 ±0.015 | 0.725 ±0.015 | 0.385 ±0.024 |
| `pooled-both` | 0.891 ±0.014 | 0.707 ±0.015 | 0.798 ±0.011 |

Pooling costs **nothing measurable** on full-window known accuracy — `pooled-c14` is 0.906 against
0.905 and 0.725 against 0.726, both well inside the draw spread. T-328's "about half a point of
full-window class accuracy" is if anything generous to the objection. The known-side price of
`pooled-both` is real but small (−0.014 family, −0.019 class); its price is the open set.

---

## 4. Curve 2 — the four cyclic dimensions (T-310's expansion, refused by T-328)

Same grid, with the four line significances carried as four `features@N` dimensions in place of
their max. (Measurement build only: `--features cyclic-dims`, off by default, nothing shipped.)

**A class's own snippet called `unknown`** — lower is better.

| arm | `full` | `burst(C14)` | `burst(both)` |
|---|---|---|---|
| `full` | 0.095 ±0.010 | **0.576 ±0.025** | 0.744 ±0.016 |
| `pooled-c14` | 0.094 ±0.013 | **0.107 ±0.014** | 0.563 ±0.030 |
| `pooled-both` | 0.110 ±0.015 | 0.119 ±0.013 | **0.176 ±0.014** |

**Held-out unknown recall, the gate's draw** — floor 0.90.

| arm | `full` | `burst(C14)` | `burst(both)` |
|---|---|---|---|
| `full` | **0.956 ±0.006 [0.949, 0.967]** | 0.973 ±0.007 | 0.894 ±0.017 |
| `pooled-c14` | **0.909 ±0.005 [0.902, 0.914]** | 0.901 ±0.012 | 0.870 ±0.015 |
| `pooled-both` | 0.716 ±0.013 | 0.709 ±0.015 | 0.605 ±0.016 |

**Known-side accuracy.**

| arm | right family, `full` | right class, `full` | right family, `burst(both)` |
|---|---|---|---|
| `full` | 0.904 ±0.010 | 0.725 ±0.008 | 0.186 ±0.011 |
| `pooled-c14` | 0.905 ±0.013 | 0.723 ±0.013 | 0.414 ±0.029 |
| `pooled-both` | 0.880 ±0.014 | 0.695 ±0.017 | 0.803 ±0.012 |

**T-328's two conclusions both hold, and both are confirmed by the wider sample.** At the shipped
fitting protocol the expansion changes known accuracy by nothing measurable (0.904 vs 0.905 family,
0.725 vs 0.726 class — "the win does not reach the classifier") and makes the burst case clearly
worse (0.576 vs 0.383). That is the refusal, and it stands at this fitting protocol. **At a pooled
fit the sign flips**, which is the reversal T-328 flagged as a footnote.

---

## 5. Which options are distinguishable from the floor

Two questions, and they have different answers. Both are reported because conflating them is how
this ticket's original framing went wrong.

- **Is the mean above 0.90?** Compare against the standard error over eight bases (sd/√8).
- **Will one gate run clear 0.90?** The gate draws once. Compare against the **draw sd** — and note
  how many of the eight draws actually fell below.

| feature set | arm | truncation | mean | draw sd | (mean−0.90)/sem | draws < 0.90 | one-run risk |
|---|---|---|---|---|---|---|---|
| max | `builtin` | `full` | 0.9530 | 0.0084 | +17.8 | 0 / 8 | ~0 |
| max | `builtin` | `burst(both)` | 0.8949 | 0.0159 | −0.9 | **5 / 8** | **~0.6** |
| max | `pooled-c14` | `full` | **0.8911** | 0.0112 | **−2.3** | **7 / 8** | ~0.8 |
| max | `pooled-c14` | `burst(both)` | 0.8535 | 0.0174 | −7.6 | 8 / 8 | ~1.0 |
| max | `pooled-both` | `full` | 0.7071 | 0.0043 | −128 | 8 / 8 | 1.0 |
| four | `full` | `full` | 0.9561 | 0.0057 | +27.8 | 0 / 8 | ~0 |
| four | `pooled-c14` | `full` | **0.9088** | 0.0053 | **+4.7** | **0 / 8** | **~0.05** |
| four | `pooled-c14` | `burst(both)` | 0.8696 | 0.0148 | −5.8 | 8 / 8 | ~1.0 |
| four | `pooled-both` | `full` | 0.7159 | 0.0130 | −40 | 8 / 8 | 1.0 |

**Read it like this.**

- **`max` + `pooled-c14` is not "at the floor", it is below it.** T-328's single draw read 0.9000,
  which is why the ticket said "cannot be distinguished from the floor". Over eight draws the mean
  is **0.8911 ±0.0112**, which is 2.3 standard errors *below* 0.90, and **seven of eight individual
  draws fail**. The extra sample did not leave this ambiguous; it resolved it against the option.
- **`four` + `pooled-c14` clears the floor and is distinguishable from it** — +4.7 sem, 0 of 8
  draws below. But the margin is small in absolute terms (0.0088, ~1.7 draw sd), so roughly **one
  gate run in twenty would read below 0.90**. That is a real operational cost of adopting it, and it
  is honest to state it rather than to quote 0.9088 as if the gate would see that number.
- **Everything on `pooled-both` fails by an enormous margin** — 0.707/0.716 against 0.90, tens of
  standard errors out, every draw. There is no sampling question here.
- **The shipped classifier already fails the floor on genuine short bursts** — `builtin` /
  `burst(both)` at 0.8949 with 5 of 8 draws below. It is *not distinguishable from* the floor,
  before any change is made. The gate does not see this because the gate scores full-length records.

---

## 6. The options

Stated with what each buys and what it costs, in the units above. **No recommendation is made here**
and the floor is not moved in any of them.

### Option 1 — change nothing

Keeps held-out unknown recall at 0.953 ±0.008 on full windows. Leaves 70.3 % of genuine short bursts
rejected from their own class, and leaves the open set on those same bursts at 0.895, already
indistinguishable from the floor. Costs nothing; fixes nothing.

### Option 2 — adopt the four cyclic dimensions **and** pooled-c14 fitting, together

The only configuration measured that buys a real piece of the gap and stays above the floor.

- Buys: `burst(C14)` own-class rejection **0.576 → 0.107**; right family on that population
  0.420 → 0.892. So an emitter C14 got a short look at is no longer refused its own class.
- Costs: held-out unknown recall **0.956 → 0.909 ±0.005**. Above the floor and distinguishably so,
  but with ~5 % of single gate runs reading below it.
- Does **not** buy: genuinely short emissions. `burst(both)` rejection only moves 0.744 → 0.563, and
  the open set on them falls to 0.870, below the floor on every draw.
- Known-side cost: none measurable (family 0.905, class 0.723).
- Note the two halves are a package: pooled-c14 with the **shipped** single dimension lands at
  0.8911, *below* the floor. The expansion is what pays for the pooling.

### Option 3 — adopt pooled-both fitting

The only configuration measured that actually fixes genuine short bursts: `burst(both)` rejection
**0.703 → 0.179** (max) or 0.744 → 0.176 (four dims), and right family on bursts 0.242 → 0.798.

It costs held-out unknown recall **0.953 → 0.707**. That is not a floor question; it is a different
classifier. Widening every class enough to contain an eighth of a record makes the taxonomy absorb
the unlisted generators wholesale, which is the exact failure CLAUDE.md's "unknown signals are the
priority" exists to prevent. **Taking this means the floor moves, which the standing rule forbids.**
It is listed because it is the measured answer to "what would it take to accept short bursts with
this feature set", and the answer is: more than the open set can pay.

### Option 4 — refuse both, and fix the length dependence at source instead

The measurement points at this and does not price it. The structure of the numbers says the
remaining gap is not in the densities:

- `pooled-c14` removes the C14-window dependence almost completely (`burst(C14)` 0.384 → 0.111,
  against a full-window floor of 0.094) — the symbol-derived dimensions stop being window-dependent.
- The genuinely-short case barely improves (0.703 → 0.580). So **most of what rejects a real burst
  is not the cyclic dimensions at all** — it is the other ~20 features' own dependence on the
  snippet length, which T-328 did not measure because its protocol never shortened the snippet.
- Widening the densities to absorb that (Option 3) is what costs 0.25 of open-set recall. Making
  those features length-invariant, as T-312/T-313 did for the feature FFT length, would cost
  nothing in open set by construction.

This would be a new ticket, not a choice among the curves above.

---

## 7. What was measured and is not on the table

- **No floor moved.** ADR-0016 §7's 0.90 is used exactly as the gate uses it, over the gate's own
  population and predicate.
- **Nothing was adopted.** `data/densities-1.json` and `densities-below-gate-1.json` are untouched;
  every refit in this document happened in memory inside the measurement binary.
- **The four-dimension feature set is behind a cargo feature** (`cyclic-dims`) that is off by
  default and never on in CI. A default build's `FEATURE_NAMES` and every default-build feature
  vector are byte-for-byte what they were. `tests/feature_length_invariance.rs` enumerates
  `FEATURE_NAMES` and does not hold under that feature, by construction.
- **One additive API**: `Classifier::with_models(model, below_gate)`, so an experiment can refit
  both shipped models under one protocol instead of a hybrid. Nothing in the pipeline calls it.

## 8. Re-deriving this

```text
just t364-curves                 # both feature sets, 8 seed bases, ~40 min total
just t364-curves --bases 16      # tighter spreads
```

The binary prints every table above and writes a JSON report carrying each individual draw, so a
later reader can recompute a spread rather than trust a quoted digit — which is the whole point of
ADR-0016 §7.2.

**When quoting anything from here, quote the population and the spread.** `0.9088 ±0.005` and
`0.8911 ±0.011` are different answers to the same question about two different configurations;
`0.9088` and `0.9000` compared as bare digits are not an answer to anything.
