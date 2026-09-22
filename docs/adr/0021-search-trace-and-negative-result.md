# ADR-0021 — The search trace and the negative result: what MAUTO tried, what it refused to try, and what `unknown` means

**Status:** PROVISIONAL (T-549 + T-550, 2026-09-21, core interface, planning only). No code comes from this ADR; MAUTO is unscheduled until after M3.
**Touches:** C13/C14 estimation, C15 classifier, C17 priors, C18 signatures, C21 bit framing, C27 inventory; Emitter, Decode, CandidatePipeline ([docs/07 §2.11, §2.15, §2.28](../07-data-model.md)).
**Builds on:** [ADR-0015](0015-decoder-synthesis-contracts.md) (the search, the stage ladder, the verdict vocabulary, `/api/analyze`), [ADR-0016 §7/§8](0016-classification-contracts.md) (open-set `unknown`, the MAUTO seed, the blind evaluation protocol), [docs/17](../17-burst-recall-vs-open-set.md) (what open-set honesty costs when it is measured properly), [ADR-0011](0011-decoder-workbench-contracts.md) (the block catalogue), [ADR-0018](0018-gnss-known-code-exception.md) (the crate-boundary confinement pattern reused in ADR-0021 §9.3).
**Amends:** ADR-0015 §1.3 (what pruning keeps), §3.4 (the result object), §5.1/§5.2 (routes and the job object), §5.4 (attach), §7 (the negatives row), §8 (`unsupported-structure`), §10 (the M-3/M-8/M-9/M-11/M-12 scope), §11.1 (`CandidatePipeline`).

**Why a new ADR rather than ADR-0015 §13.** The trace and the negative result are a *contract consumed outside the search* — by `/api/analyze`, by the inventory, by ADR-0016's open-set work and by the acceptance suite — and ADR-0015 is already 672 lines carrying two in-file amendments; a third and fourth would bury a core-interface contract inside a document about beam search. ADR-0015 §10 carries a pointer to here.

**Reference convention.** A bare `§n` refers to **ADR-0015**, the document this one amends throughout. This ADR's own sections are always written `ADR-0021 §n`. Where a section number exists in both (§2.3, §4.1, §5, §8, §11.1 …) that prefix is the only thing distinguishing them, so it is never dropped.

---

## Context

ADR-0015 reports **the winner** well. `PipelineResult` (§3.4) carries a full stage ladder with per-stage metric, raw value, support `n`, bits and quality; §3.4 gives ranked partial results with backend-rendered summaries; §5.2's `progress` carries live counts.

It does not report **the search**. The beam keeps only "the best pruned node" (§1.3); `progress` carries four bare integers (`stage_max`, `beam`, `pruned`, `deferred`); everything else the engine considered and rejected is discarded at the moment of the decision. Nothing in §10's M-1…M-14 sketch owns the record: M-3 builds the beam and prunes, M-8 serves the API, M-11 renders the results panel, and between them the reason a hypothesis left the search is never produced by anyone.

So the shipped system could say "FSK at 4800 Bd" and could not say **why not PSK** — or whether PSK was looked at at all. Those are different statements, and the project's own rules already say they must not read alike:

- CLAUDE.md's canvas rule: **grey = genuinely unobserved**, and observed-but-not-yet-measured is a distinct mark, not grey (T-441). A hypothesis family that was *deferred for budget* is the decode-side grey; a family that was *tried and measured below floor* is the decode-side measurement. Rendering them identically is the same defect.
- CLAUDE.md's honesty rule: **the UI never implies detail the front end cannot deliver.** A single confident verdict with no visible alternatives implies a search that looked everywhere.
- CLAUDE.md's priority: **unknown signals are the priority to surface and catalogue.** An engine that converts an unknown into a plausible wrong label does not merely fail to help — it destroys the thing the product is for, invisibly, because a wrong `framed` verdict looks exactly like a right one.

ADR-0015 guards one direction of that last failure (§4.2 reserves ≥ max(P(unknown), 0.2) of the budget for open skeletons and defers rather than deletes low-posterior families) and guards the other only with a *measurement*: §7's "the verdict is above truth in ≤ 1 % of cases". That is a report card, not a mechanism. The only mechanism is the look-elsewhere subtraction `L_j`, and T-547 questions whether the quantity being subtracted from is a log-probability at all.

This ADR specifies the two halves together because they are one thing: **the trace says what was tried; the resolution says what the result means when nothing won.**

---

## Decision summary

| Question | Decision |
|---|---|
| What the trace records | **Decisions, not evaluations.** One `TraceNode` per hypothesis that entered the frontier and left it, carrying the hypothesis, the stage, the measured evidence, and a **closed `outcome` enum** split into *tried* and *not-tried* kinds. A grid sweep collapses into one `swept` descriptor on its parent. |
| Tried vs not tried | The load-bearing distinction, carried structurally (`measured` is null for every not-tried kind), never inferred by the client. |
| Bound | `max_trace_nodes` (128/512/2048 by profile) and `max_trace_bytes` (256 KiB). Applied **on insert**, so residency is bounded at all times. |
| What is dropped | A fixed priority order (ADR-0021 §2.3). Not-tried nodes, the winner's lineage and the best node per (stage, family) are **never** dropped. Every dropped node increments an `elided` counter keyed by (stage, family, outcome), so the trace is **complete in counts and lossy in detail, never the reverse.** |
| Owner | **M-3**, inside the beam. A beam that has thrown the information away cannot have it retrofitted, only re-instrumented. §1.3's "the best pruned node is kept" is superseded. |
| API | `trace_summary` on `AnalyzeJob` (always, small); the full trace at a new `GET /api/analyze/{id}/trace` with `?stage`/`?outcome`/`?family` filters; a `trace` stream record at most once per stage completion. |
| Determinism | Replayable under a recorded `replay_key`, **except** decisions whose outcome is `deferred_budget` or a `plateau` stop, which carry `nondeterministic: true`. Acceptance tests bound by `max_evaluations`, never by wall. |
| Negative result | A sealed **`Resolution`** object beside the verdict, with `kind` ∈ {`unknown`, `structured-unidentified`, `unsupported-structure`, `not-searched`}, a closed `reason` enum of five, a **coverage** block (what was searched, how far, what was not), and a `retry` block. |
| `not-searched` ≠ `unknown` | The decode-side statement of the coverage-map rule. An emitter with no finished job reads `not-searched`; only a finished job writes `unknown`. |
| Guard against manufacture | The look-elsewhere term is **not sufficient**. An in-job **shuffled-null control** (ADR-0021 §8.2) measures the same search's own propensity to find structure in structureless data and can only *cap* a verdict, never raise one. |
| False-label budget | **≤ 1 false label per 1 000 analyze jobs** on signal-free or out-of-catalogue input, at every profile; false *confirms* stay on T-548's budget and this ADR may not loosen it. Measured by `acceptance_mauto::negative_control` over four negative populations, asserted as **0 observed at n = 800**, with the Wilson interval printed rather than implied. |
| Database interaction | A C17 suggestion attaches as `explanations[]` **beside** a sealed `Resolution`, after it is final, and may modify nothing else. Enforced by the crate graph (`hk-synth` cannot see `hk-context`), with a boundary test in the ADR-0018 style. |

---

# Part A — The search trace (T-549)

## 1. What the trace is, and what it must not become

**The trace is a record of the engine's decisions.** A `deep` job may run tens of thousands of evaluations (§3.3); the trace is bounded at ~10² decisions. Three rules keep it that way:

1. **One node per frontier event**, not per evaluation. A node is created when a hypothesis enters the frontier and finalised when it leaves (survived, pruned, deferred, refined away).
2. **Continuous sweeps collapse.** Grid points around a seed (`the estimate ×{½, 1, 2} ± 3 steps of 1 %`, §3.1 step 3) and refinement iterations (§2.3) are **not** nodes. They are a `swept` descriptor on the node that owns them, plus that node's `evaluations` count. Only a sweep that *changes the structural choice* creates a sibling.
3. **Memoised prefix hits are nodes but carry no measurement** — they name the node they reused, which is how a reader sees that the engine did not pay twice.

**The accounting identity**, asserted by a test: `Σ node.evaluations over recorded nodes + Σ elided[*].evaluations == job.used.evaluations`. The trace names a subset of the decisions and **accounts for all of the work**.

## 2. The `TraceNode`

### 2.1 Shape

```jsonc
{ "id": "n17", "parent": "n4",                        // null at the root
  "stage": "S1",                                       // ADR-0015 §1.1 ladder, unchanged
  "hypothesis": {
    "skeleton": "generic-fsk-framed@1",                // or a template id
    "slot": "S1", "choice": "fsk",                     // the slot alternative this node fixes
    "family": "fsk",                                   // the hk-mod@1 family, for filtering ("why not PSK")
    "params": { "deviation_hz": 2400, "symbol_rate_bd": 4800 },
    "swept": [ { "path": "nodes[clock].params.symbol_rate_bd",
                 "lo": 4700, "hi": 4900, "points": 7, "scale": "log" } ] },
  "seed_source": "estimate",                           // estimate|template|classification|signature|proposal|open
  "prior_bits": -1.2,                                  // reported; never ranks (§1.3), never sorts the trace
  "measured": {                                        // NULL iff `outcome` is a not-tried kind
    "metric": "bimodality", "raw": 0.71, "n": 4096,
    "bits": 4.1, "quality": 0.40,
    "floor_bits": 6.0, "look_elsewhere_bits": 2.3 },
  "evidence_bits": 7.1,                                // cumulative prefix score after look-elsewhere; null if not tried
  "outcome": "pruned_floor",
  "outcome_detail": { "floor_bits": 6.0, "measured_bits": 4.1 },   // typed per outcome; numbers, never prose
  "tried": true,                                       // derived, but SERVED, so no client infers it
  "evaluations": 7, "cpu_ms": 41,
  "summary": "2-FSK at 4800 Bd: discriminator bimodality 4.1 bits, below the 6-bit S1 floor" }
```

`summary` is backend-rendered, matching §3.4's rule that the UI does no wording logic.

### 2.2 The `outcome` enum (closed; adding a variant is a contract change)

**Tried** — the node was evaluated, `measured` is non-null, `tried: true`:

| Outcome | Meaning | `outcome_detail` |
|---|---|---|
| `survived` | stayed in the beam and was expanded | `{children: n}` |
| `pruned_floor` | stage-*j* `b_j` < `floor_j` (§1.3) | `{floor_bits, measured_bits}` |
| `pruned_bound` | optimistic bound below the best complete result | `{bound_bits, best_bits}` |
| `pruned_beam` | scored, but outside beam width W or the 2-per-family diversity cap | `{rank, width, cause: "width" \| "diversity"}` |
| `evaluated_worse` | a complete candidate that finished below the winner | `{rank, gap_bits}` |
| `refined_into` | replaced by its refined child (§3.1 step 6) | `{into: "n21"}` |
| `memoised` | prefix-hash hit; reused another node's stage output, no new work | `{reused: "n9"}` |

**Not tried** — `measured` is null, `evidence_bits` is null, `tried: false`. These are the honesty-critical rows and they are **never elided** (ADR-0021 §2.3):

| Outcome | Meaning | `outcome_detail` |
|---|---|---|
| `deferred_prior` | posterior < 0.02, sent to the side queue (§4.2 "defer, don't delete"), budget never reached it | `{posterior, queue_position}` |
| `deferred_budget` | enqueued and ready, but a budget/wall/evaluation cap hit first | `{stop, queue_position}` |
| `unsupported` | no block exists for the structure (§8) | `{structure: "psk", missing_block: "psk_demod", ref: "ADR-0011 §1.5"}` |
| `not_applicable` | the slot alternative is inconsistent with a fixed ancestor choice | `{conflicts_with: "n4"}` |
| `refused_power` | the power policy refused the expansion (§3.3) | `{policy: "battery"}` |

**The distinction is structural, not stylistic.** `deferred_budget` says *we did not look*; `pruned_floor` says *we looked and it measured 4.1 bits against a 6-bit floor*. A user deciding whether to spend a `deep` budget needs exactly this difference, and it is the same distinction the canvas draws between grey and a measured tile.

### 2.3 The bound, and what falls off it

An unbounded trace is a memory leak on a handheld, and §3.3's constraint binds: work on the search thread competes with the ring.

| Profile | `max_trace_nodes` | `max_trace_bytes` |
|---|---|---|
| `quick` | 128 | 256 KiB |
| `standard` | 512 | 256 KiB |
| `deep` | 2 048 | 256 KiB |

At ~200–400 bytes per node including its rendered `summary`, **the byte bound binds first at `deep`** (2 048 × 400 ≈ 800 KiB). That is intended: the node cap is the semantic limit, the byte cap is the hard one, and both are applied **on insert**, so residency never exceeds the bound even transiently.

**Never dropped**, in this order of protection:

1. the root, and **every ancestor of a node in `results[]`** (the winner's lineage *is* the explanation of the winner);
2. **every not-tried node** — there are few of them (one per deferred family, one per unsupported structure, not one per evaluation) and they are the rows that cannot be reconstructed from anything else;
3. the **best node per (stage, family)** pair, whatever its outcome — so "the best PSK hypothesis reached S1 at 3.2 bits" survives even when 400 PSK nodes do not;
4. at least one node per distinct `unsupported.structure`.

**Dropped first**, in this order:

1. `memoised` nodes (no new measurement);
2. `pruned_beam`, ascending by `evidence_bits` (the most obviously worse first);
3. `pruned_bound`, ascending;
4. `pruned_floor` beyond the best two per (stage, family);
5. `evaluated_worse` beyond rank 10.

**Nothing is silently lost.** Every drop increments an `elided` bucket:

```jsonc
"elided": [ { "stage": "S3", "family": "psk", "outcome": "pruned_floor",
              "count": 412, "bits_max": 4.6, "bits_min": 0.2, "evaluations": 1648 } ]
```

So the trace is **complete in counts and lossy in detail, never the reverse.** "412 PSK hypotheses were tried at S3, the best measured 4.6 bits against a 6-bit floor" is a complete and honest statement; a missing row is not. The client is told `truncated: true` and `nodes_elided`, and must show it — the same rule as a pane stating the honesty tier it was actually drawn at.

## 3. Who produces it: M-3, and what changes there

**The trace is M-3's, inside the beam.** ADR-0015 §10's M-3 row ("Search engine: beam, memoisation, pruning, budget/profiles, stop rules, power/throttle") is widened rather than a ticket added after it, because the information is destroyed at the decision site: every `continue` in a prune loop is a fact that has to be written before the loop moves on.

**Amendments to M-3's scope:**

- A `TraceSink` is threaded through the search. **Every** site that removes a node from the frontier, refuses to add one, or skips one calls it. There is no "prune quietly" path; that is the point.
- §1.3's sentence *"Pruning is never total: the best pruned node is kept as a partial result"* is **superseded**. The best pruned node is still promoted to a partial result; it is now also one trace node among the retained set, and it is no longer the only thing kept.
- The retention policy of ADR-0021 §2.3 lives in the sink and runs on insert.
- §5.2's `progress` keeps its four counters (they are the cheap live number) and gains `tried`, `not_tried` and `elided`, so a live reader is never misled by a `pruned` count that silently mixes both kinds.
- The per-node cost is a **measured** budget item, not an assumed-free one: T-453's constraint applies here too. M-3 reports the trace's wall and allocation cost as a fraction of the search, and the ADR's claim that it is negligible is a claim to be checked, not asserted.

`seed_source`, `family` and the not-tried outcomes all exist upstream of M-3 (in the §4.2 seeding step and the ADR-0016 `SearchSeed`), so **M-1's type definitions must carry them from the start** — a second amendment, to M-1, listed in ADR-0021 §12.

## 4. How the trace crosses the API

Per T-079, `docs/api.md` and `crates/hk-cli/tests/api_contract.rs` move together; the stream record additionally moves `docs/stream-contract.md`. **The deltas are listed here, not written.**

### 4.1 On the job object (always present, small)

`AnalyzeJob` (§5.2) gains:

```jsonc
"trace_summary": {
  "decisions": 18422, "nodes_recorded": 512, "nodes_elided": 17910, "truncated": true,
  "by_outcome": { "survived": 41, "pruned_floor": 302, "pruned_bound": 96, "pruned_beam": 51,
                  "memoised": 14, "evaluated_worse": 4, "deferred_budget": 3, "unsupported": 2 },
  "by_stage": [ { "stage": "S1", "tried": 61, "not_tried": 2, "best_bits": 11.4 } ],
  "replayable": false }
```

This is what polling and the inventory read; it is a few hundred bytes and it answers "did it look, and how much of what it looked at can I still see".

### 4.2 The full trace is a separate fetch

| Method | Path | Query | Answers |
|---|---|---|---|
| GET | `/api/analyze/{id}/trace` | `?stage=`, `?outcome=`, `?family=`, `?tried=true\|false`, `?limit=` (≤ 512) | `{job_id, engine, replay_key, bounds: {max_nodes, max_bytes, truncated}, nodes: [TraceNode], elided: [ElidedBucket]}` |

**Why not inline it.** `GET /api/analyze/{id}` is polled and every stream record is an idempotent snapshot (§5.2); a 256 KiB trace on each one is a waste on a battery device and would be shipped repeatedly. The filters exist so the "why not PSK" panel fetches only the rows it renders.

**Errors:** `404 not_found` — no such job. `410 gone` — the job ran and has been forgotten (the §5.1 fifty-job window) or its trace was evicted with it. Those are different facts and get different codes: *we forgot* is not *it never ran*, which is the same distinction the trace itself exists to preserve.

### 4.3 On the stream

A new `trace` record on `hackriff.analyze/1`, emitted **at most once per stage completion** — never per decision. It carries that stage's `by_outcome` counts, its `tried`/`not_tried` split, and the ≤ 8 highest-bits nodes at that stage. Records drop (§5.2), so the stream is a progress hint and **the `GET` is always authoritative**.

### 4.4 What persists, and what does not

| Object | Lifetime |
|---|---|
| `nodes[]` | **Job lifetime only.** Not persisted: a node table growing per analyze per emitter buys little and costs a schema. |
| `trace_summary` | **Persisted** on the `emitter_synthesis` row (§5.4), append-only, so a second look reads the history of what earlier looks covered. |
| `replay_key` | **Persisted** on the same row. Without it a summary cannot be compared to a later one. |
| `CandidatePipeline.trace_ref` | A `synthesis`-origin row (§11.2) gains `{job_id, node_id}`, so a competing hypothesis in the inventory points back at the decision that created it, for as long as the job is held. |

## 5. Determinism: when a trace can be argued with

A trace that cannot be reproduced cannot be argued with, so the ADR states the exact condition rather than claiming determinism generally.

The job records a **`replay_key`**:

```jsonc
{ "engine": "hk-synth@1.4.0", "templates": [{"id":"pocsag","version":1}],
  "blocks": [{"name":"fsk_demod","version":2}], "calibration_hash": "…",
  "window": { "clip_id": "rec_…", "sha256": "…" },     // the pin-on-analyze clip (ADR-0015 §6) makes this exact
  "profile": "deep", "budget": { "max_evaluations": 20000 }, "seed_ref": "…" }
```

- **Deterministic**, given an identical `replay_key`: the beam order, memoisation, every prune, every score, and therefore every `survived` / `pruned_*` / `evaluated_worse` / `unsupported` / `not_applicable` / `deferred_prior` outcome. Beam search with memoisation is deterministic *by choice* (ADR-0015 Options), and the pin-on-analyze clip makes the input byte-exact.
- **Not deterministic across machines**: any decision whose cause is a wall-clock or CPU-time cap — `deferred_budget`, and a `plateau` stop measured over "25 % of the budget". Those nodes carry `nondeterministic: true` and `trace_summary.replayable` is `false` for the whole job.
- **Therefore:** the acceptance suite bounds jobs by `max_evaluations` and **never by wall**, so its traces are exact and a regression in the search is visible as a trace diff rather than as a flaky pass. This is a testability requirement on M-12, listed in ADR-0021 §12.

## 6A. What the UI can show — and what it must not compute

M-11 renders the results panel; the trace makes "why not PSK" answerable there **with no signal logic in the client** (CLAUDE.md's thin-client rule). Everything the UI does is filtering, grouping and formatting over already-decided fields.

**The panel.** Per stage, a column of rows in the backend's order, each showing the backend's `summary`. Two visual registers, decided by the served `tried` boolean — never by the client inspecting `measured` or parsing an outcome string:

- **tried and rejected** — a measurement, rendered as one (bits against floor);
- **not tried** — rendered distinctly, as the decode-side grey.

**"Why not PSK?"** is `GET /api/analyze/{id}/trace?family=psk`, and the answer is whatever comes back:

| Rows returned | What the user reads | The action offered |
|---|---|---|
| `unsupported {structure: psk, missing_block: psk_demod}` | "PSK was not tried: this build has no PSK demodulator." | none — and the emitter joins the `psk_demod` backlog (ADR-0021 §9.4) |
| `deferred_budget {stop: budget}` | "PSK was queued and never reached: the `standard` budget ran out at S3." | **re-run at `deep`** |
| `deferred_prior {posterior: 0.004}` | "PSK ranked below 0.02 and waited; the search finished first." | re-run at `deep` (the open-search floor guarantees it gets a share) |
| `pruned_floor {measured 3.2, floor 6.0}` | "PSK was tried: constellation evidence 3.2 bits, below the 6-bit floor." | none — this is an answer |
| nothing | **must not happen.** The backend emits an explicit `not_applicable` root node for a family absent from the skeleton set, so the panel says "PSK is not in this search's skeleton set" rather than rendering an empty box | — |

That last row is the same rule as the canvas's: **an empty surface is never the answer**; a surface may be empty only where the backend has said, in data, that nothing exists.

**Truncation is shown.** `nodes_elided` and `truncated` are rendered beside the list, with the `elided` buckets as a tail row ("412 more PSK hypotheses tried at S3, best 4.6 bits"). A shorter list with no note would imply a smaller search — exactly the defect a fixed-size placeholder is.

## 6B. Worked example: a near miss the user can read

A 915 MHz ISM sensor, `standard` profile, open search (`templates.off`). Rank 1 solves; **rank 2 is the half-rate hypothesis**, the classic symbol-rate harmonic ambiguity that `EmissionFeatures` already carries as an alternative (ADR-0016 §8). As the panel renders it:

```
Result 1 — solved · 91 bits · 2-FSK, 4800 Bd, CRC-16/IBM
  S1 fsk        discriminator bimodality 0.79   n 9600   11.2 bits  ✓ (floor 6)
  S2 clock      eye openness 0.84                n 9600   11.0 bits  ✓
  S3 nrzi       bit structure ok                 n 9600    7.4 bits  ✓
  S4 sync 0x2DD4  38 hits vs 0.4 by chance       n 38     31.8 bits  ✓ (floor 10)
  S5 crc-16     37 distinct valid of 38          n 37     52.0 bits  ✓  → hold-out 74 bits
  look-elsewhere charged: −11.4 bits

Result 2 — framed · 34 bits · 2-FSK, 2400 Bd  (half the rank-1 rate)
  S1 fsk        same demodulator, same node reused (memoised from n9)
  S2 clock      eye openness 0.71                n 4800    8.1 bits  ✓  — opens on every OTHER symbol
  S3 nrzi       bit structure ok                 n 4800    6.2 bits  ✓
  S4 sync 0x2D  12 hits vs 1.1 by chance         n 12     14.9 bits  ✓  — the 8-bit head of the real sync
  S5 crc-16     1 distinct valid of 12           n 1       4.8 bits  ✗ pruned_floor (floor 10)
  "Half-rate hypothesis: frames on a truncated sync, one check passes in twelve — consistent with
   chance at this width. The 4800 Bd hypothesis explains the same bits with 37 of 38 checks valid."
```

This is the shape the trace is for. The near miss is **not noise**: it is a real structure that a real estimator would propose, it reaches S4 with genuine excess sync hits, and it dies where it should — at the check, against the floor, with the numbers shown. Nothing here requires the client to know what NRZI is.

---

# Part B — The negative result (T-550)

## 7A. `unknown` is a positive, durable finding

### 7A.1 Why not a new verdict value

§3.4's `verdict` ladder (`solved` → `checked` → `framed` → `clocked` → `demodulated` → `energy`) says **how deep the search got**. It is not the place for "what this means", and adding `unknown` to it would create two names for one state — `energy` and `unknown` — and push the choice between them onto a client.

Instead: the ladder is unchanged, and the job and the emitter gain a **`Resolution`**, present whenever no result reached `solved`.

### 7A.2 The `Resolution`

```jsonc
"resolution": {
  "kind": "unknown",                    // unknown | structured-unidentified | unsupported-structure | not-searched
  "deepest_verdict": "framed",          // the §3.4 ladder, unchanged
  "reason": "budget-exhausted",         // the closed enum in §7A.3
  "coverage": {
    "profile": "deep", "engine": "hk-synth@1.4.0", "t": "2026-09-20T14:02:11Z",
    "skeletons": { "offered": 11, "tried": 9, "deferred": 2, "unsupported": 2 },
    "families": [
      { "family": "fsk",  "state": "tried",       "deepest_stage": "S5", "best_bits": 41.2 },
      { "family": "ook",  "state": "tried",       "deepest_stage": "S3", "best_bits": 12.0 },
      { "family": "psk",  "state": "unsupported", "missing_block": "psk_demod" },
      { "family": "ofdm", "state": "unsupported", "missing_block": "ofdm_sync" },
      { "family": "msk",  "state": "deferred",    "deferred_as": "deferred_budget" } ],
    "budget": { "spent": { "wall_s": 119.4, "cpu_s": 392.1, "evaluations": 18422 },
                "exhausted": true, "stop": "budget" },
    "hypotheses": 18422, "look_elsewhere_bits": 14.2,
    "window": { "duration_s": 2.0, "samples": 4.8e7, "snr_db": 14.1, "bursts": 12, "clip_id": "rec_…" } },
  "null_control": { "k": 8, "ran": true, "best_null_bits": 21.4, "margin_bits": 12.9, "capped": false },
  "ruled_out": [ { "template": "pocsag", "version": 1, "deepest_stage": "S2", "best_bits": 5.1 } ],
  "retry": { "worthwhile": true, "on": ["more-budget"], "not_on": ["longer-window"],
             "reason_code": "budget-binding", "not_before": "2026-09-20T15:02:11Z" },
  "trace_summary": { /* ADR-0021 §4.1 */ },
  "replay_key": { /* ADR-0021 §5 */ },
  "explanations": [ /* ADR-0021 §9.3 — attached AFTER this object is sealed, modifies nothing above */ ],
  "summary": "Analysed at deep over 2.0 s at 14 dB. 9 of 11 skeletons tried; PSK and OFDM have no
              block in this build; MSK was queued and not reached. Best: framed at 41 bits, no valid
              check. More budget is the missing ingredient, not a longer window." }
```

`coverage` is the point: it is **the decode-side coverage map**. Not-yet-looked and looked-and-found-nothing are different states, they are computed from what was actually searched, and `families[*].state` is the per-family grey.

### 7A.3 Why nothing won — the closed `reason` enum

These must be distinguishable, because they are different answers with different next actions.

| `reason` | Means | How it is distinguished, mechanically |
|---|---|---|
| `no-signal` | S0 itself measured below floor: there was not energy to analyse | `deepest_verdict: energy` **and** the S0 node's `bits < floor_0` |
| `nothing-scored` | energy existed; every hypothesis tried measured below its stage floor; the beam emptied | `stop: exhausted`, `budget.exhausted: false`, best `bits` < floor at the deepest stage attempted |
| `tied` | ≥ 2 complete candidates within the 4-bit supersession margin (§11.3) and none met the solve rule — the engine genuinely cannot choose, and naming one would be a coin flip | `results[0].evidence_bits − results[1].evidence_bits < 4`, neither `solved` |
| `budget-exhausted` | the queue was non-empty when the budget ran out — **the space was not covered** | `stop: budget` **and** `skeletons.deferred > 0` |
| `unsupported-structure` | the highest-posterior suspicion has no block | `families[argmax posterior].state == "unsupported"` |

Adding a variant is a contract change. **`no-signal` and `nothing-scored` must not be merged**: a noise window and a strong unmodulated carrier the engine cannot structure are different facts, and the guard suite (ADR-0021 §8) asserts each population lands on its own one.

### 7A.4 `not-searched` is not `unknown`

> **The invariant.** Not-yet-analysed and analysed-and-found-nothing are different states, and neither may be rendered as the other.

This is the decode-side statement of CLAUDE.md's coverage rule (grey = genuinely unobserved, and observed-but-not-measured is a different mark). Concretely:

- An emitter with **no** `emitter_synthesis` row has `resolution.kind = "not-searched"`, `coverage: null`. It is not `unknown`, it is *un-looked-at*.
- Only a **finished** job writes `unknown`. A cancelled or failed job writes `not-searched` with `last_attempt` set, because an aborted look ruled nothing out.
- `/api/inventory` serves `resolution: {kind, reason, t, profile}` on every row, so the inventory can show a **genuinely characterised unknown** as different from an un-analysed one — which is the difference between a catalogue and a list of boxes.

### 7A.5 `structured-unidentified` — the result the product should be proud of

§8 says a framed, check-valid, high-entropy signal is "characterised and left there". That characterisation is a **real result**: it ranks, it displays and it persists like one, with `kind: "structured-unidentified"` and its own object on the rank-1 `PipelineResult`:

```jsonc
"characterisation": {
  "framing": { "sync_word": "0x2DD4", "sync_bits": 16, "frame_bits": 128,
               "regularity": 0.98, "frames": 41, "bit_order": "msb", "polarity": "normal" },
  "check":   { "kind": "crc", "model": "searched", "poly": "0x1021", "width": 16, "init": "0xFFFF",
               "distinct_valid": 37, "corrected_excluded": 0, "pass_rate": 0.90, "in_catalogue": false },
  "payload": { "bytes": 14, "entropy_bits_per_byte": 7.91,
               "constant_fields": [ { "offset": 0, "len": 2, "value": "0x8A31", "frames": 41 } ],
               "counter_fields":  [ { "offset": 4, "len": 2, "step": 1, "wrap": 65536 } ],
               "identity_recurrence": 0.0 },
  "note": "framed, CRC-16 valid on 37 of 41 frames, payload entropy 7.91 bits/byte: probably
           encrypted or compressed" }
```

Three consequences, stated because they are easy to get wrong:

1. **It writes no family to Classification.** ADR-0016 §8: a failed search writes nothing; an *unidentified* one writes nothing either, because "framed" is not a family. It does write a C18 signature proposal — but with provenance **`structure-proposed`**, explicitly not the `decoder-confirmed` of §4.2's feedback path.
2. **It may confirm the emitter.** Confirm-by-decode (§5.5) is about *measurement*, not *identification*: 37 distinct valid 16-bit checks on hold-out is exactly the evidence the rule asks for. So **a Confirmed emitter with no identity at all is a legal, intended state**, and the inventory must render it as one rather than hunting for a label to put in the row.
3. **The searched-generator discount applies.** A CRC-16 that was *found* is weaker evidence than one a template *specified*, and `assist::codes` already models the chance factor. **T-548 owns that number**; this ADR records the dependency rather than inventing one, and until T-548 lands, a `structured-unidentified` result with a searched check confirms only at `deep` with the null control passed (ADR-0021 §8.2).

### 7A.6 `unsupported-structure` names what it suspected

Without this, the absence of a LoRa decode reads identically to a LoRa signal decoded as noise. So:

- `kind: "unsupported-structure"` is distinguishable from `reason: "nothing-scored"` in the served object, not by inference.
- It **names the structure and the missing block**: `{structure: "css", missing_block: "css_dechirp", suspected_by: "classification", posterior: 0.61}`. `suspected_by` ∈ `classification | features | template | operator`, so a reader can tell a confident suspicion from a shrug.
- The gap list is ADR-0011's catalogue: today `psk_demod`/Costas (ADR-0015 §1.1's stated gap, M-14) plus OFDM, DSSS, CSS and QAM (§8). **T-554 owns the block-catalogue gap list**; this ADR consumes it and adds the requirement that every entry on it has a stable `missing_block` id, so a resolution can name it and ADR-0021 §9.4's backlog can count it.

## 8. The guard against a search that always finds something

This is the half that matters. §4.2 guards priors starving the open search; **nothing guards the opposite**, and §7's "over-claim ≤ 1 %" is a measurement of the failure rather than a mechanism against it.

### 8.1 The position: look-elsewhere alone is not sufficient

The ADR takes a position rather than leaving it open. `L_j = log₂(hypotheses evaluated at stage j in this job)` is **necessary and insufficient**, for three reasons:

1. **It is per-job.** One hundred analyze jobs on noise are one hundred independent chances at the same threshold. A per-job correction cannot bound a per-session or per-emitter false-label rate. (T-548 raises the identical objection for confirmations.)
2. **It is subtracted from a quantity that may not be a log-probability.** §2.2 itself marks the calibrated nulls for bimodality, eye openness, EVM and SNR **unverified** across 8-bit quantisation and gain states, and T-547 questions the scale. A correct correction applied to a miscalibrated quantity yields a miscalibrated result.
3. **It is analytic where the failure is empirical.** The number of hypotheses *evaluated* is not the number of *effectively independent* hypotheses; a beam full of near-duplicate symbol rates is charged as if they were independent tests, and a proposal operator that returns correlated candidates is charged as if they were not.

Two alternatives were evaluated and are **not sufficient on their own**:

- *A hold-out that must independently reach the same structure.* Already in §3.1 step 7, and it stays — but it does not guard manufacture. A wrong CRC polynomial that happens to fit the framing fits **both halves**, because the error came from the search rather than from the sample. Necessary, not sufficient.
- *A verdict ceiling tied to the hypothesis count spent.* This is the look-elsewhere term again with a different shape, and it inherits the same calibration problem. A weak form is adopted in ADR-0021 §8.2 instead, tied to the control rather than to a formula.

### 8.2 The mechanism: the shuffled-null control

**What it is.** Before a job reports `solved` or `checked` from an **open** (non-template) search, the engine re-runs **the winning candidate's complete prefix, unchanged and unrefit**, over `K` null windows derived from the same IQ at the same gain state:

- a **time-reversed** copy of the analysed window;
- **phase-randomised surrogates** — the same magnitude spectrum with randomised phase, which preserves PSD (and so SNR and occupied bandwidth) and destroys symbol timing and framing;
- where the coverage map says one exists, an **adjacent in-band window with no detection**.

`K` by profile: `quick` 0 (quick never confirms), `standard` 2, `deep` 8. Charged to the job's own budget at ≤ 5 % of `max_evaluations`.

**What it does.** It measures **this search's own propensity to find structure in structureless data**, which is exactly the quantity `L_j` estimates analytically and may estimate wrongly — and it measures it on the same front end, the same gain state and the same quantisation, so it is self-calibrating against the failure mode §2.2 says the tables cannot be trusted for.

**The rule.** Let `b_null` be the best evidence the winning prefix reaches on any null window.

- If `holdout_bits − b_null < min_null_margin` (**8 bits**, 256:1), the verdict is **capped at `framed`**, no confirmation occurs, and the resolution becomes `kind: "unknown"`, `reason: "tied"`, with `null_control: {k, best_null_bits, margin_bits, capped: true}`.
- The control can **only cap**. It never raises a verdict, never adds bits, never revives a pruned node. A mechanism that could raise a verdict would be a second way to manufacture one.
- It is recorded **whether or not it fires**. "The null control ran and passed with a 12.9-bit margin" is a visible, checkable fact; an absence is not.

**The falsifiability clause.** The guard suite reports how many negative-population jobs were saved *by the control* — jobs that would have reported ≥ `framed` without it. **If that number is zero across the whole suite, the control is dead weight and this ADR's §8.2 should be reconsidered**, because a mechanism that never acts is a cost with no evidence behind it. That is a claim about the mechanism that a measurement can refute, which is the standard §7 asks of everything else.

### 8.3 The false-label budget

A **label** is a verdict ≥ `framed` reported to the user. A **confirm** is a lifecycle change. They have different budgets because they have different blast radii.

> **Budget.** At most **one false label per 1 000 analyze jobs** on signal-free or out-of-catalogue input, at every profile. False **confirms** stay on **T-548's** budget; this ADR may not loosen it, and where the two disagree, the stricter binds.

**The assumption behind 1/1000, stated because it is judgement and not measurement:** a handheld left running with auto-analyze (§10 open question 3) plausibly issues 10²–10³ jobs a day over a busy band. One false label per 1 000 jobs is about one a day at the top of that range — roughly the most a user can be asked to discount and still treat the list as a catalogue rather than a suggestion box. If auto-analyze is never adopted, the number is conservative; if job rates go higher, it must tighten.

### 8.4 The guard test: `acceptance_mauto::negative_control`

> **The corpus this suite runs on is specified in [docs/22](../22-mauto-acceptance-corpus.md)** (T-551, frozen 2026-09-21): the four populations below plus an **N5 mismatched-hypothesis** population, the per-claim trial counts, the sufficiency argument and the coverage manifest. It amends nothing here.

**Protocol, unchanged from §7 and non-negotiable:** fixtures replay **through the mock SDR** behind the ordinary device interface; jobs start via `POST /api/analyze`; targets come from **blind detection** (an inventory emitter or burst the run found) or an ad-hoc band — **never a truth frequency**. Truth is loaded only by the assert harness. Jobs are bounded by `max_evaluations`, never by wall (ADR-0021 §5), so the suite is deterministic.

**Four negative populations, because "nothing to find" has four shapes that fail differently:**

| | Population | n | Content | Must return |
|---|---|---|---|---|
| **N1** | thermal noise | 400 windows | mock-SDR noise at the fixture's gain state, no emission | `kind: unknown`, `reason: no-signal`, `deepest_verdict: energy`; **0 labels ≥ `framed`**; 0 emitters created |
| **N2** | energy without symbols | 200 | CW carrier, analog FM voice, AM voice | `kind: unknown`, `reason: nothing-scored`; `deepest_verdict ≤ demodulated`; **0 ≥ `framed`** |
| **N3** | out-of-catalogue structure | 200 | synthetic OFDM (non-standard CP), DSSS, CSS/LoRa-like, 16-QAM — **real** structure, no block | `kind: unsupported-structure`, **naming** the structure and `missing_block`; **0 `solved`**; `framed` permitted only where the trace shows a *measured* framing (a real preamble is a real measurement) |
| **N4** | real empty capture | the 433 MHz empty capture + the 50 Ω terminator capture (**T-375**) | real front end, real noise, real spurs | as N1, **and a spur is never a label**: attributed via `artifact_of` (§11.4), never a verdict ≥ `demodulated` |

**The counted quantity.**

```
false_labels = #{ jobs over N1 ∪ N2 ∪ N4 whose rank-1 verdict ≥ framed
                  and whose resolution.kind ∈ {unknown, not-searched} }
```

N3 is scored **separately**, because finding a real preamble in a real OFDM signal is a true measurement, not a false label; what N3 asserts is that it is never `solved` and never silently `unknown` when the structure was suspected.

**The assertion, and what it honestly bounds.** With n = 800 across N1/N2/N4 at each of `standard` and `deep`, a 1/1000 budget permits **zero** failures, and the test asserts `false_labels == 0`. That is the whole of what is asserted, and the ADR states the limit plainly: **0 of 800 is consistent with a true rate up to ≈ 0.0037 at 95 % confidence**, so the suite supports the 1/1000 claim only to that precision. Quoting "one in a thousand, measured" from a zero at n = 800 would be exactly the four-decimal error docs/17 §1 documents.

**What is reported alongside**, so drift is visible while the assertion stays at zero:

- `labels_per_1000` per population with a **Wilson interval**, per profile;
- the **null-control save count** (ADR-0021 §8.2's falsifiability clause);
- the distribution of `resolution.reason` per population — a population drifting from `no-signal` to `nothing-scored` is a real change in the engine even when no label is emitted;
- best `evidence_bits` reached per population, which is the early-warning number: it rises long before a label appears.

**Relation to §7's existing negatives row.** §7 asserts "noise, CW, analog FM voice and synthetic OFDM give 0 `solved`; 200 noise windows at `deep` give 0 `solved`". That row is **subsumed and strengthened**: `0 solved` becomes `0 labels ≥ framed` (strictly harder), the populations widen from 200 to 800, and real captures join the synthetic ones. §7's standing rule holds — the M-12 review may tighten these, never loosen them after seeing results.

## 9. The known-signal database, and where it may not reach

CLAUDE.md: the database is never the starting point, never pre-populates the inventory, never overrides what was measured. The failure this section prevents is narrower and more specific than that rule: **a suggestion turning an `unknown` into a label.**

### 9.1 Where a suggestion attaches

Exactly two places, and no others.

1. **Into the search**, as `prior_bits` on a hypothesis, in `hk_synth::seed` (§4.2). This already exists and is already bounded: priors order the search and **never** rank a result or confirm one (§1.3); a template's `bands_hz` "only raise rank for an emitter already detected there" (§4.1); the open-search floor (§4.2) and ADR-0016 §8's "order by posterior, prune by likelihood" keep a prior from pruning anything.
2. **Onto the result**, as `resolution.explanations[]` — computed by `hk-context` **after** the `Resolution` is final, from the **measured** parameters only:

```jsonc
{ "source": "licence",                 // alloc | band-plan | licence | catalogue | history
  "identity": "…", "score": 0.61,
  "distance_hz": -150000, "status": "unexpected",      // expected | unexpected | no_reference_data
  "data_age_days": 41,
  "reasoning": "FM broadcast allocation; nearest assignment 150 kHz below the measured centre" }
```

### 9.2 What it may and may not modify

| May modify | May **not** modify |
|---|---|
| `resolution.explanations[]` | `resolution.kind`, `resolution.reason`, `resolution.coverage`, `resolution.null_control` |
| — | `verdict`, `stage_reached`, `evidence_bits`, `prior_bits`, `check`, `characterisation` |
| — | the emitter's measured `f` / `BW`, any lifecycle state, any `Classification` row |

**The rule in one line: a suggestion explains a result; it never becomes one.** An `unknown` with three ranked explanations is still `unknown`, and the UI renders the explanation list **subordinate to** the verdict — never in its place, never as the row's headline.

**The mismatch stays a flag.** An emitter measured 150 kHz off the FM raster gets `status: "unexpected"` and keeps its measured centre. That is the interesting case (CLAUDE.md), not an error to correct.

### 9.3 Enforced structurally, not by convention

A rule of this kind that lives only in prose gets broken by the first convenient edit. So:

- `Resolution` is **constructed and sealed** by `hk-synth` before any context lookup runs. `hk-pipeline::synth` passes `hk-context` an immutable `&Resolution` and receives back only a `Vec<Explanation>` to attach.
- **`hk-synth` does not depend on `hk-context`** in the ADR-0015 §9 crate graph, and must not — so the search cannot read a suggestion even accidentally, and no future edit inside `hk-synth` can reach one.
- A **boundary test** asserts that direction, in the same spirit as ADR-0018's guard confining the GNSS known-code exception: the confinement is a test, not a promise.
- The negative-result path **adds no new database read**. Everything ADR-0021 §9.1's first bullet already allowed remains; nothing new is opened.

### 9.4 The `missing_block` backlog

`unsupported-structure` resolutions are queryable: *"3 emitters are waiting on `psk_demod`, 1 on `css_dechirp`"*. This costs nothing (it is a group-by over `emitter_synthesis`) and is a real product statement — and it is the strongest available argument for ADR-0015 §10's optional M-14, made from the device's own data rather than from a guess about what users will meet.

## 10. When an unknown is re-tried, and when re-trying is burning battery

A finished `unknown` is re-analysed only when **something measurable has changed**, and `resolution.retry` names which. The rule is served, not inferred.

| `retry.reason_code` | Condition | Policy |
|---|---|---|
| `budget-not-binding` | `stop: exhausted`, queue empty — the search covered the space and found nothing | **More budget buys nothing.** Never auto-retry. Retry only when the `replay_key` changes: a new block, a new template, a new engine version. |
| `budget-binding` | `stop: budget` with `skeletons.deferred > 0` | More budget is exactly what is missing. Offer `deep`; auto-retry only with spare scheduler capacity and the emitter still present. |
| `snr-improved` | the emitter's measured SNR is ≥ 3 dB above the analysed window's | Retry: a real change in what can be measured. |
| `more-support` | accumulated bursts have at least doubled, or a continuous emitter's retained extent is ≥ 2× the analysed one | Retry: every significance figure is a function of `n`. Doubling `n` is worth roughly a bit on rate-like metrics — modest, real, and the only honest reason to re-read the same signal. |
| `unsupported` | `kind: unsupported-structure` | **Never retry until the named block exists.** Re-running the same catalogue against a known-unsupported structure is the definition of burning battery. |

**Hard floors, whatever the reason code:** no automatic re-analysis of the same emitter within `min_retry_interval` (default 1 h), and none at all under the `battery` power policy (§3.3).

**One distinction that must not be conflated.** CLAUDE.md's *"overlap is an error signal that triggers re-analysis"* is about **detection**: overlapping boxes mean the time–frequency analysis is wrong, and what re-runs is the detector. This section is about the **decode search**. An overlap re-analysis does not imply a new analyze job, and a `budget-not-binding` unknown is not re-searched because two boxes overlapped.

---

## 11. Deltas (listed, not written)

### 11.1 `docs/api.md` + `crates/hk-cli/tests/api_contract.rs` (T-079: they move together)

| Route | Delta |
|---|---|
| `GET /api/analyze/{id}` | `AnalyzeJob` gains `trace_summary` (ADR-0021 §4.1) and `resolution` (ADR-0021 §7A.2). `progress` gains `tried`, `not_tried`, `elided`. |
| **`GET /api/analyze/{id}/trace`** (new) | ADR-0021 §4.2: filters `stage`/`outcome`/`family`/`tried`/`limit`; `404 not_found` vs `410 gone`. |
| `POST /api/analyze` | `null_control` may be disabled only in a test build; there is no request field for it. |
| `GET /api/inventory` | each row gains `resolution: {kind, reason, t, profile}`; filter `?resolution=not-searched\|unknown\|structured-unidentified\|unsupported-structure`. |
| `GET /api/inventory/{id}/pipelines` | a `synthesis`-origin row gains `trace_ref: {job_id, node_id}` (ADR-0021 §4.4). |
| `docs/stream-contract.md` | `hackriff.analyze/1` gains the `trace` record (ADR-0021 §4.3); `done` carries the `resolution`. |

### 11.2 `docs/07` data model

- **New §2.29 `Resolution`** — the ADR-0021 §7A.2 object: kind, reason, coverage, null control, retry, the `not-searched` rule (ADR-0021 §7A.4), retention (append-only with the job's `emitter_synthesis` row), and the rule that `explanations[]` is attached after sealing and modifies nothing else.
- **§2.11 Emitter** — gains `resolution` (latest, summarised); an emitter with no `emitter_synthesis` row reads as `not-searched`, never `unknown`; a Confirmed emitter may carry no identity (ADR-0021 §7A.5).
- **§2.28 CandidatePipeline** — a `synthesis`-origin row gains `trace_ref`; a `structured-unidentified` result is a first-class ranked row.
- **`emitter_synthesis`** (ADR-0015 §5.4) — gains `resolution` (JSON), `trace_summary`, `replay_key`, `null_control`. Append-only, so a second look reads the *history* of what was ruled out.
- **§2.21 Classification** — one clarifying sentence: a `structured-unidentified` result writes **no** family (ADR-0016 §8), and proposes a C18 signature with provenance `structure-proposed`, not `decoder-confirmed`.

### 11.3 ADR-0015 amendments this ADR makes

| ADR-0015 § | Change |
|---|---|
| §1.3 | "the best pruned node is kept" is superseded by ADR-0021 §2: it is still a partial result and is now one retained trace node among the set. |
| §3.4 | `PipelineResult` gains `characterisation` (ADR-0021 §7A.5) for `structured-unidentified` results. |
| §5.1/§5.2 | the routes and job fields in ADR-0021 §11.1. |
| §5.4 | `emitter_synthesis` gains four columns (ADR-0021 §11.2). |
| §5.5 | an open-search result may not confirm unless the null control ran and passed (ADR-0021 §8.2); the searched-generator discount defers to **T-548**. |
| §7 | the negatives row is subsumed and strengthened by ADR-0021 §8.4. |
| §8 | `unsupported-structure` gains `missing_block` and `suspected_by`, and a queryable backlog (ADR-0021 §9.4). |
| §10 | M-1, M-3, M-8, M-9, M-11 and M-12 gain scope; see ADR-0021 §12. |
| §11.1 | `CandidatePipeline` gains `trace_ref`. |

---

## 12. Task-graph amendments (ADR-0015 §10)

The M-n ids are ADR-0015's sketch and are not board entries; the board entries that carry these amendments are filed with this ADR.

| M-n | Amendment |
|---|---|
| **M-1** | The `TraceNode`, `outcome` enum, `Resolution` and `reason` enum are declared here, and the seeding types carry `seed_source` and `family` from the start (ADR-0021 §3). |
| **M-3** | **Owns the trace.** `TraceSink` at every frontier-removal site; retention on insert; `progress` gains the tried/not-tried split; per-node cost is measured, not assumed (ADR-0021 §3). |
| **M-8** | `GET /api/analyze/{id}/trace`, `trace_summary` and `resolution` on the job, the `trace` stream record, `404` vs `410`; `docs/api.md` + contract tests together (ADR-0021 §4, §11.1). |
| **M-9** | The `Resolution` is sealed by `hk-synth`; attach persists it with `trace_summary` and `replay_key`; the null control gates open-search confirmation (ADR-0021 §7A.4, §8.2, §9.3). |
| **M-11** | The trace panel: tried vs not-tried registers, the family filter, truncation shown, the re-run action (ADR-0021 §6A). |
| **M-12** | `acceptance_mauto::negative_control` over N1–N4; `max_evaluations`-bounded jobs; Wilson intervals and the null-control save count reported (ADR-0021 §5, §8.4). |
| **M-14** | Motivated, not required, by ADR-0021 §9.4's backlog. |

---

## Options considered

- **A per-evaluation log, filtered at read time.** Rejected: tens of thousands of records per `deep` job on a handheld, written on the thread that gates the ring (T-453's constraint). The trace records decisions and accounts for the rest in counts.
- **An unbounded trace with compression.** Rejected: an unbounded structure is a memory leak whose size depends on how hard a user searched, which is the wrong thing for residency to depend on.
- **`unknown` as a sixth verdict on the §3.4 ladder.** Rejected: it would give one state two names (`energy` and `unknown`) and push the choice onto a client. The ladder says how deep; the `Resolution` says what that means.
- **LRU eviction of trace nodes.** Rejected: recency is uncorrelated with explanatory value. The priority order in ADR-0021 §2.3 protects the rows a reader cannot reconstruct — the not-tried ones and the winner's lineage.
- **No additional over-claim mechanism; the look-elsewhere term is enough.** Rejected, with reasons, in ADR-0021 §8.1: it is per-job, it is subtracted from an unverified quantity, and it counts evaluations rather than effectively independent tests.
- **A verdict ceiling as a function of hypothesis count.** Rejected as the primary mechanism: it is the look-elsewhere term in another shape and inherits the same calibration problem. A weak form survives, tied to the null control (ADR-0021 §8.2).
- **Measuring over-claim only in the suite (today's §7 position).** Rejected: a measurement is a report card. The suite stays and strengthens (ADR-0021 §8.4), but a mechanism now sits in front of it.
- **Letting a high-scoring C17 suggestion resolve a `tied` result.** Rejected outright: it is the precise failure CLAUDE.md's blind-first rule exists to prevent, and the crate graph is arranged in ADR-0021 §9.3 so it cannot be done by accident.

## Consequences

- The search becomes inspectable: "why not PSK" has five distinct, served answers, and three of them carry an action.
- `unknown` becomes a durable, dated, budgeted assertion rather than an absence, so a second look knows what the first ruled out — and the inventory can distinguish a characterised unknown from a box nobody has analysed.
- A framed, check-valid, unidentified emission becomes a first-class catalogued result that can confirm an emitter with no identity attached.
- Costs: one additional route and a stream record; four columns on `emitter_synthesis`; a bounded structure in the beam whose per-node cost M-3 must measure; and up to 5 % of every open-search job's budget spent on nulls that, by design, usually find nothing.
- The false-label budget is a **stated assumption** (ADR-0021 §8.3), and at n = 800 the suite asserts zero rather than proving 10⁻³. Saying so is the point.

## Open questions (for the user)

1. **The false-label budget.** Is 1 per 1 000 jobs the right bar, given it implies ~1/day under continuous auto-analyze? Tighter costs recall on genuinely weak-but-real signals.
2. **Null-control cost.** 5 % of an open-search budget, always, on a battery device — or only at `deep`, accepting that `standard` open results then confirm on the look-elsewhere term alone?
3. **The 8-bit null margin.** 256:1 over the best of K nulls is a first guess, like every threshold in ADR-0015 §7. It should be measured in M-12 against the N1–N4 populations before it is treated as a number.
4. **Trace retention.** Job-lifetime nodes plus a persisted summary (proposed), or persist the full node list for the last N jobs so a trace can be re-read days later?
5. **Auto-retry.** Should ADR-0021 §10's rules fire automatically at all, or should every re-analysis stay user-triggered until auto-analyze (ADR-0015 open question 3) is decided?

*Unverified in this ADR: the 128/512/2048 node caps, the 256 KiB byte cap, K = 4/2/8, the 8-bit null margin, the 1/1000 false-label budget, the 3 dB and 2× retry thresholds, and the 1 h retry floor. All are first guesses in the ADR-0015 §7 tradition, to be measured in M-3 and M-12 and never loosened after seeing results.*
