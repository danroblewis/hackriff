# ADR-0015 — Decoder synthesis contracts: candidate pipelines, stage evidence, search, templates, region analyze

**Status:** ACCEPTED (2026-09-23, user decisions U1–U5). Written PROVISIONAL (T-208, core interface); accepted on the §16 review (T-848) with the user's answers to [docs/20](../20-mauto-decision-brief.md)'s U1–U5 — U1 B, U2 B, U3 A, U4 A, and **U5 yes (stereo audio in scope, overriding the brief's recommendation)**. §16.2's eight engineering corrections are folded into the sections they amend; §16.8 lists what the acceptance did **not** decide and still stands open. The M-1 scaffold (`crates/hk-synth`, `hk_model::synth`: types and stubs, no engine) exists; the engine lands in M-2…M-12. Changing this ADR now goes to Fable plus the user.
**Amended by:** [ADR-0022](0022-false-confirm-budget.md) (§5.5, §11.5, §1.3 — the confirm gate) and **§13 below** (T-616/T-617/T-618, 2026-09-21 — evidence-bit dependence, calibration reach, ADC-fill conditioning); **§14 below** (T-557, 2026-09-22 — template fact provenance, the fact/implementation line, bulk import).
**Touches:** C13/C14 estimation, C15 classifier, C18 signatures, C20 digital demod, C21 bit framing, C22 decoders, C27 inventory; Emitter, Decode ([docs/07 §2.11, §2.15](../07-data-model.md)).
**Builds on:** [docs/15](../15-decoder-synthesis.md) (the design brief), [ADR-0011](0011-decoder-workbench-contracts.md) (blocks, recipes, stream), [ADR-0012](0012-attention-memory-contracts.md) (chain tiers), ADR-0014 (IQ capture ring; T-178, `docs/adr/0014-iq-capture-ring.md` lands with it), ADR-0016 (M3 classification; T-198, written in parallel).
**Planned code:** new crate `hk-synth` (search, evidence scoring, templates; no HTTP), `hk-pipeline::synth` (jobs, acquisition, attach), `crates/hk-api/src/analyze.rs` (replaces the T-190 stub), `templates/*.template.json`.

## Context

Detection finds energy; it does not say which decoder turns that energy into data. docs/15 frames this as **search**: synthesize the pipeline structure and parameters that best explain a signal. The search is guided by cheap per-stage "getting warmer" evidence, seeded by blind estimators, templates, the M3 classification and C18 signatures. It ends in **confirm-by-decode**.

M1 already provides most of the parts: **recipes** with hot edit and block `Status` (ADR-0011); **authoring assist** (T-091, `hk_estimate::assist`: sync, period, CRC/BCH and field search with absolute, chance-corrected scores and a work `Budget`); the **refinement loop** (T-070, `hk_demod::refine::{Objective, RefinementLoop}`); the **inventory lifecycle** (T-078, `hk_pipeline::inventory::ConfirmPolicy`); the **burst detector** (T-075) and the **IQ ring** (T-157/T-178). This ADR fixes the contracts that join them, and defines the real `POST /api/analyze` (T-190 ships a validating 501 stub).

Constraints carried in: **blind-first** (templates and priors only order the search, the measured decode decides, and priors never veto evidence, ADR-0016's rule); tune from the processed output; a classical core with ML at most a heuristic slot; capture never blocks; thin UI; one developer, so no speculative plugin surfaces.

## Decision summary

| Question | Decision |
|---|---|
| What a candidate is | A **recipe prefix** (ADR-0011 document, valid up to its deepest stage) plus typed **free parameters**. The winner is an ordinary recipe. |
| Structure space | A closed stage ladder S0–S6. Each template or open **skeleton** offers alternative node sub-chains per stage slot. |
| Evidence unit | **Significance bits against a noise/wrong-hypothesis null**, per stage. Bits are comparable across stages and capped per stage. The engine subtracts the look-elsewhere cost (log₂ hypotheses tried). |
| Evidence API | New optional `Block::evidence()` summary. It is not a port. Diagnostic ports stay for visuals. Status records mirror it as `<node>.ev_*`. |
| Prior vs evidence | Kept in **separate numbers** (`prior_bits`, `evidence_bits`). Priors order the search; only evidence ranks the result and confirms it. |
| Search | Staged beam search with prefix memoisation, floor pruning and optimistic-bound pruning. Assist runs as **proposal operators** for huge discrete spaces (sync words, CRC polynomials, field maps). T-070's loop, through an `EvidenceObjective`, does local refinement of continuous parameters. |
| Budget | Per-job wall, CPU, evaluation and cache caps. Profiles `quick`/`standard`/`deep`. Power-aware admission. A new `synth` chain kind below real-time chains. |
| Stopping | Solved (≥ 64 evidence bits on hold-out, ≥ 3 distinct frames), budget, plateau, beam exhausted, cancelled. There are always ranked partial results. |
| Templates | JSON `hackriff.template/1`: a recipe reference or skeleton, free-parameter ranges, measured-parameter priors and plausibility checks. Built-in plus user and discovered. Never truth. |
| M3 interface | Reads the `Classification` family distribution (including `unknown`) and `SignatureMatch`. Writes a decode label and a signature proposal back. Open search keeps a budget floor. |
| Analyze API | `POST /api/analyze` → `202` job. Get, list and cancel. `hackriff.analyze/1` messages stream. Results attach to the emitter. Save as template. |
| Confirm-by-decode | New `ConfirmPolicy.synthesized` rule: hold-out validation, evidence ≥ 64 bits, check width ≥ 16. Decodes are provenance-marked `synthesized`. |
| Bursts | Burst sets from the ring (ADR-0014), joined with `DISCONTINUITY`. Pin-on-analyze clip. A single frame confirms only through ADR-0022's one inequality — in practice a template-fixed check of ≥ 24 bits (U1 = B, user 2026-09-23); searched checks never on one frame. |
| Evaluation | Blind through the mock SDR, with hidden truth. Templates-on and templates-off runs. A-priori solve and over-claim floors. |

## 1. Candidate pipelines and the staged objective

### 1.1 Stage ladder

`hk_synth::Stage` is closed; adding a stage is a contract change.

| Stage | Nodes (ADR-0011 catalogue) | Free parameters (typical) | Primary evidence | Source |
|---|---|---|---|---|
| S0 channel | runtime DDC, `mix`, `lowpass`, `resample` | centre, bandwidth, rate | in-band SNR and occupied fraction vs adjacent guard bands | runtime channel stage (new `channel.snr_db`) |
| S1 demod | `fm_demod`, `am_demod`, `fsk_demod`, `msk_demod`, `subcarrier` (`ppm_demod` spans S1–S4) | family, deviation, offset, subcarrier | FSK/FM: discriminator bimodality and offset/deviation; AM/OOK: envelope bimodality; pilot/subcarrier lock | block `Status.lock`/`snr_db` + evidence |
| S2 clock | `clock_recovery` | symbol rate, pulse, algorithm, loop bandwidth | eye openness, timing-error variance, lock | `clock_recovery` (`timing_error` diagnostic) |
| S3 bits | `slicer`, `diff_decode`, `nrzi`, `manchester` | threshold, invert, line code | soft-symbol bimodality/EVM, Manchester violation rate, bit-structure sanity (neither constant nor all-toggle) | slicer / line-code blocks |
| S4 framing | `sync_search`, `deframe`, `assemble`, `interleave` | sync word or offset words, polarity, bit order, frame length rule | sync hits in excess of chance, inter-sync regularity (assist `significance_bits`) | `sync_search` |
| S5 check | `crc`, `bch`, `parity`, `checksum` | RevEng model, BCH (n, k, poly), span | distinct valid frames × check width (assist `evidence_bits`) | check blocks |
| S6 fields | `fields`, `text` | field map | fit ok/partial share, identity recurrence across frames, template plausibility ranges | `fields` |

**Catalogue gap — closed (C5, U4).** Generic PSK is funded: M-14 is **required** (U4 = A, user 2026-09-23), and its block landed as **T-609** — `psk_demod` in `hk-blocks` (BPSK/DBPSK, QPSK/DQPSK, π/4-DQPSK, OQPSK, 8PSK/D8PSK; carrier recovery, matched filter, soft per-bit output; additive to ADR-0011 §1.5), with T-610 `viterbi` and T-611 `reed_solomon` as the rest of the same investment. What M-14 still owes beyond T-609 is listed in §10's M-14 row. **SSB and CW get no blocks**: they stay on the legacy Listen chain permanently (§12.5), as a decision, not a gap.

### 1.2 Candidate representation

```jsonc
{ "skeleton": "generic-fsk-framed@1",            // or a template id
  "choices": { "S1": "fsk", "S3": "nrzi" },      // slot alternative fixed so far
  "recipe": { /* hackriff.recipe v2 document: nodes for S0..S_k only,
                 one `stage` output on the deepest node */ },
  "free": [ { "path": "nodes[clock].params.symbol_rate_bd",
              "domain": { "float": { "lo": 4700, "hi": 4900, "scale": "log", "resolution": 0.001 } },
              "seed": 4800, "source": "estimate" },            // estimate|template|classification|signature|proposal|open
            { "path": "nodes[sync].params.sync_word",
              "domain": { "proposal": "assist.sync" } } ] }
```

- Every beam node is a **valid, runnable recipe prefix**: `Recipe::validate` passes, and the tail is a `stage` output. "Synthesized pipelines are recipes" holds at every step, not only at the end. A result can be started (`POST /api/pipelines {recipe, target}`), hot-edited and saved exactly like a hand-written one.
- `free` paths use ADR-0011 node ids and parameter names. Domains: `float`/`int` (range, resolution), `enum` (prior weights), `hex` (candidate list), or `proposal`, naming a proposal operator (§3.2). Huge discrete spaces are never enumerated.
- Structure alternatives live in the skeleton, not the recipe schema: `slots: {S3: [{id: "none", nodes: []}, {id: "nrzi", nodes: [...]}, ...]}`.

### 1.3 Node score

For a prefix reaching stage k:
- `evidence_bits = Σ_{j≤k} min(b_j, cap_j) − L_j`
  - `b_j` is the stage-j significance (§2). **Amended by §13.1 (T-616):** within a stage, `b_j` is the sum over *declared dependence groups* of the **maximum** within each group, not a sum over metrics — `snr` and `evm` as the M1 FSK block computes them are one statistic (ρ = 0.989), and undeclared means one group. **Amended by §13.2 (T-617):** no calibrated metric may claim above 6 bits or above its table's `admissible_bits`, so the 12-bit S0–S3 cap is a belt and the 32-bit S4 cap is analytic-only; `floor_j` snaps **up** to an expressible level and is otherwise `floor_unreachable`;
  - caps default to 12 bits for S0–S3, 32 for S4, and no cap for S5–S6;
  - `L_j = log₂(hypotheses evaluated at stage j in this job)`: the look-elsewhere cost, charged by the engine.
- `prior_bits = log₂ π(h)`, clipped to [−8, 0] (§4.2).
- The **search order** key is `evidence_bits + prior_bits + optimistic_remaining`.
- The **result rank** key is `(stage_reached, evidence_bits)`. Prior bits are reported but never rank a result and never confirm one.
- **Floors** prune: a node whose stage-j `b_j` < `floor_j` (default 6 bits S0–S3, 10 bits S4–S5) is pruned. Pruning is never total: the best pruned node is kept as a partial result.
  - **S6 has no floor** (C7): `default_floor_bits(S6) = None`; S6 evidence only ranks (§4.1, "plausibility … ranking evidence only").
  - **A floor is never lowered to fit a table** (C8): `Threshold` snaps **up** to an expressible level; a floor above a table's `admissible_bits` is `floor_unreachable` and is reported, not relaxed. (Whether 6 bits at S3 is reachable at all under §13.1's maximum rule is open — §16.8 item 1.)
- **Why bits.** An SNR in dB, an eye opening and a CRC pass rate aren't comparable. Their tail probabilities under a null are. Caps stop a very strong carrier (S0) from outranking a weaker signal that actually frames. The look-elsewhere term keeps a search that tried 10⁴ CRC hypotheses from "finding" one by chance, the failure the assist docs describe for three Mode-S frames.

## 2. Evidence-metric API

### 2.1 Blocks

`hk_blocks::Block` gains one optional method (default: none):

```rust
fn evidence(&self, out: &mut EvidenceSet) {}   // between chunks; no allocation; ≤ 4 entries
pub struct Evidence { stage: Stage, metric: MetricId, raw: f32, n: u32, bits: f32, quality: f32 }
```

- `raw` is the metric in its natural unit.
- `n` is the support: symbols, bursts or distinct frames.
- `bits` is the significance against the block's null for that `n`.
- `quality = 1 − e^(−bits/8)` ∈ [0, 1], for display.
- `MetricId` is a closed enum: `snr`, `bimodality`, `offset_ratio`, `pilot_lock`, `eye_open`, `timing_var`, `evm`, `line_violations`, `bit_structure`, `sync_excess`, `sync_regularity`, `check_distinct_valid`, `field_fit`, `identity_recurrence`, `plausibility`.
- **Not a port.** The search needs one summary per evaluation window, not a per-item stream. Ports would cost buffers and taps. Diagnostic ports (`timing_error`, soft symbols) stay the visual path.
- **Counters are windowed.** The engine calls `reset()` between windows, and `evidence()` covers everything since. Check blocks count distinct valid frames in a bounded hash set (frame-rate allocation is allowed, ADR-0011 §1.4).
- **Live mirror.** The runtime writes `evidence()` into status ticks as `<node>.ev_bits` and `<node>.ev_quality`. Running pipelines then show the same ladder the search used (ADR-0011 §1.3 keys; minor stream addition).

### 2.2 Normalisation to bits

- **Analytic nulls where they exist:**
  - sync excess: Chernoff/Poisson tail minus pattern width (the assist `significance_bits` formula);
  - check: `width × distinct_valid`, with the posterior and chance-factor handling of `assist::codes` for searched generators;
  - field fit and identity recurrence: binomial against random frames.
- **Calibrated nulls otherwise:** bimodality, eye openness, EVM and SNR. Each block `name@version` ships `synth/calibration/<block>.json`: quantiles of `raw` under noise and a mismatched-parameter null, at stated `n`.
  - The tables are generated by a `py/` research script over synthetic noise (orchestration only) and checked by a Rust test that re-samples 1 000 noise windows.
  - A block version bump invalidates its table.
  - **The schema, the conditioning key and the refusal are §13 (T-616/T-617/T-618).** A table is per (metric, null, `n`, **ADC-fill bucket**), declares the bit levels it can actually express and the `N` it was sampled with, and **refuses** a level it cannot express rather than returning the nearest quantile. The original text follows, and the phrase "gain states" in it is superseded by "ADC fill states" (§13.3):
  - Whether these tails are stable enough across 8-bit quantisation and gain states was **unverified**; **measured by T-547** ([docs/21](../21-evidence-bits-under-quantisation.md), 2026-09-21): gain state alone moves them by nothing (a no-ADC control reproduces the float table to 0.02 bits across 51 dB); the **ADC fill level** moves them, and under-fill (σ < 0.5 LSB), not clipping, is the hazard. Conditioned on fill, the spread is δ = 1.8 bits at a claimed 6 and 3.3 at a claimed 8. That note also finds that `snr` and `evm` here are one statistic (ρ = −0.989), that `eye_open`'s table is not invertible at 4 and 6 bits, and that a table of N windows cannot express more than log₂ N bits — which prices the 12-bit caps of §1.3.
- The engine, not the block, subtracts the look-elsewhere cost, because blocks don't know how many hypotheses ran.

### 2.3 Refinement reuse (generalising T-070)

`hk_synth::EvidenceObjective` implements `hk_demod::refine::Objective`:
- `space()` maps the candidate's continuous free parameters to `ParameterSpace` centre and bandwidth plus `Tuning.mode` axes (deviation, symbol rate, loop bandwidth) by name.
- `evaluate(window, tuning, depth)` runs the prefix and returns `Measurement { quality: evidence_bits, locked: deepest b_k ≥ floor_k }`.
- `EvalDepth::{Acquire, Track, Validate}` map to the short, search and hold-out windows.

`RefinementLoop`, `Termination` and hysteresis are reused unchanged. The WFM objective stays as the specialised S1 evidence for broadcast FM.

Recipe `refine.objective` gains `{"evidence": "deepest"}`. That is a new optional key, so `schema_version` 3 (ADR-0011 §2.4 rule). A running synthesized pipeline then keeps tuning from the same evidence.

**Proposed amendment (T-858 = M-7, 2026-09-24) — PENDING THE USER; not accepted, not implemented.** `hk_synth::objective` implements §2.3 as written (S0 counted in `quality`; any prefix accepted). One exception is proposed for the user to accept or reject (the coordinator raises it), with one reading of the text and one measured limit:
- **Proposed exception (not in the code) — report S0 but do not count it in `quality` or the lock.** Every calibration null is white noise at the prefix's input rate fed to its own S0 filter, and the tables are tight (`lowpass@1` credits 6 bits at +0.1 dB, n = 16 384). Measured: a channeliser whose passband rolls off at the band edges made pure noise score 6 bits at S0 — so S0, a whiteness test of the objective's own channel output, is not evidence about the emission. The proposal would also refuse a prefix reaching only S0. Consequence of the code as accepted: S0's ~6 noise bits are added to every tuning's `quality` (roughly constant, so comparisons hold), and an S0-only prefix locks on S0's floor on pure noise; deeper prefixes lock on their deepest stage and are unaffected.
- **Reading — the bandwidth axis is the S0 channel filter's width, behind a flat channeliser.** Measured: a channeliser narrower than the S0 filter's support coloured the noise the S1/S2 blocks saw, and noise locked at S2 **on hold-out**. So the down-converter is fixed at a flat 0.9 × the prefix rate and `ParameterSpace.bandwidth` moves the prefix's first `lowpass` node's `cutoff_hz` (width = 2 × cutoff), kept inside the flat passband — "maps the candidate's free parameters to centre and bandwidth", with the S0 cutoff as the bandwidth parameter. Recipe schema 3 accepts `bandwidth_hz` in `refine.tune` with the same meaning. (A filter cutoff other than the 6 kHz the tables were drawn at scores against a mismatched null, §2.2 — not measured here.)
- **As written, no change:** `EvalDepth::{Acquire, Track, Validate}` read the short leading part of the search window, the whole search window, and the hold-out only; the objective enforces the split, so `locked` means locked on hold-out. "Deepest" is fixed at construction (the candidate's deepest stage, or the result's `stage_reached`; S6 locks on S5's floor, §16.2 C7). Calibrated supports are aligned per block (each measurement re-runs the prefix over leading sub-slices sized to land each calibrated block on a table support, ≤ 1 + 2 × blocks runs).

**Measured limit, for the same decision.** Calibrated S1–S3 metrics saturate at 6 bits each, so for a strong unshaped 2-FSK the objective is flat as soon as one tone passes the S0 filter (and at low SNR the true centre scored *lower* than a one-tone tuning); a refinement at S2 finds the emission, not its centre — only analytic S4/S5 evidence ranks finer tunings. §3.1 step 6's "refine at the first S2 lock" should be weighed against that before M-3's engine calls it; the engine-side call (`refined_into`) needs an IQ-backed `Evaluator` and is not wired yet.

## 3. Search strategy

### 3.1 Loop

1. **Acquire** the analysis window (§5.3, §6) and split it: **search** (the first 60 %, or odd bursts) and **hold-out** (the rest). Channelise once to the S0 seed.
2. **Seed** hypotheses (§4.2): ranked templates, then open skeletons per plausible family, each with `prior_bits`.
3. **Expand by stage**, k = 0…6. For each beam node:
   - choose the slot alternatives;
   - grid the stage-k continuous parameters coarsely around the seed (symbol rate: the estimate ×{½, 1, 2} ± 3 steps of 1 %; centre and deviation ± their estimator uncertainty);
   - call proposal operators for `proposal` domains.
4. **Evaluate** children on the search window. Stage outputs are **memoised by prefix hash**, so a child runs only its new stage (LRU, `max_cache_bytes`, default 256 MiB).
   - Evaluation uses a batch driver over the hk-blocks graph (no ring reader): an additive `run_window` beside the T-088 graph builder.
5. **Score and prune** (§1.3):
   - beam width W = 8 at S0–S2, 4 at S3–S6;
   - at most 2 beam slots per identical family and skeleton (diversity);
   - optimistic-bound pruning: drop a node if `score + Σ remaining caps` < the best complete result;
   - deferred families (§4.2) wait in a side queue.
6. **Refine** locally (§2.3) when a node first reaches S2 lock, and again at S5 partial. The refined child replaces its parent.
7. **Validate** the top 3 complete candidates on the hold-out window at `EvalDepth::Validate`. Only hold-out evidence counts toward `solved` and confirmation.

### 3.2 Proposal operators

These are thin adapters over `hk_estimate::assist`, run on the node's own bits or frames. Their bounded `Budget` is charged to the job.

| Operator | Wraps | Proposes |
|---|---|---|
| `assist.sync` | `analyze_stream` | `sync_search` fragments (sync word or offset words), period, block codes |
| `assist.codes` | `search_codes` | `crc`/`bch` fragments with `evidence_bits`, posterior share, `ambiguous_with` |
| `assist.fields` | `suggest_fields` | a draft field map and length-field hints (`length_from`) |

Each suggestion's `fragment` becomes a child node's parameters. Suggestion `score` feeds `prior_bits`, never `evidence_bits`; the child's measured evidence decides.

### 3.3 Budget, power and admission

`SynthBudget { wall_s, cpu_s, max_evaluations, max_iq_samples, max_cache_bytes, threads }`.

| Profile | wall | CPU | threads | use |
|---|---|---|---|---|
| `quick` | 3 s | 3 s | 1 | context-menu default; templates and the top-2 open skeletons |
| `standard` | 20 s | 40 s | 2 | "Analyze" default |
| `deep` | 120 s | 400 s | 4 | opt-in; open search |

The numbers are **unverified guesses**, measured in M-3 on the Mac and later on the Jetson. **Open (§16.8 item 2):** T-552's measurement (docs/27, in flight) finds proposal-operator calls (0.3–2.4 s each) dominate wall time, and recommends budgeting search by operation/evaluation count with wall time kept only as a backstop; the profile *shape* is therefore not settled by this acceptance.
- **Chain kind `synth`** in the per-run chain budget (ADR-0011 §1.4 rule 5). Search threads run at lower OS priority than ring readers.
  - If the run's `lost_samples` rises while a job runs, the job throttles (`state: throttled`, threads halved) before capture is hurt.
- **One running job** by default and a queue of ≤ 4. Beyond that: `503 busy`.
- **Power.** The job reads the run's power policy (ADR-0007/0009): `battery` refuses `deep` (`422 power`) and halves threads, `low` allows only `quick`, and a thermal-throttle flag pauses expansion. (No such policy input exists in code yet; M-3 adds a minimal one, and U2 makes it a real input — below.)
- **Auto-analyze (U2 = B, user 2026-09-23).** The device **may start analyze jobs itself**: the ADR-0012 attention scheduler auto-queues unexplained (unknown) candidates at **`quick` only**, **on mains only**, **one auto job at a time**, sharing the one-running-job / queue-of-4 admission above (an auto job never displaces a user job). `standard` and `deep` stay **user-triggered**; `deep` stays refused on battery. The policy is a config field `auto_profile: quick | none`, product default `quick`; M-3 may ship it `none` until the attention→queue wiring and the power-policy object exist, and flipping it is configuration, not code. M-11 gets the queue view and a stop control.
- **Stopping** (first to hit): `solved` (§5.5); `plateau` (no best-score gain > 1 bit over 25 % of the budget); `budget`; `exhausted` (beam and deferred queue empty); `cancelled`; `source_ended` or `evicted`.
- **ML slot.** `trait NodeHeuristic { fn prior_bits(&self, node: &NodeView) -> f32 }`. The classical default is used, and an ML value function (via the ADR-0016 ml-runtime) may replace it later. It only reorders; it never scores evidence.

### 3.4 Ranked partial results

`PipelineResult`: `{rank, verdict, summary, recipe (concrete, every free parameter bound), template ({id, version} | null), stage_reached, stages: [{stage, node, metric, raw, n, bits, quality}], evidence_bits, prior_bits, check: {kind, model, width, pass_rate, distinct_valid, tested, holdout}, frames_preview (gated, ≤ 8 inspector frame records)}`.

`verdict`: `solved` (S5 on hold-out meets the solve rule), `checked` (S5 partial: "reached sync, 40 % CRC"), `framed` (S4), `clocked` (S2–S3), `demodulated` (S1), `energy` (S0 only). `summary` is backend-rendered text, so the UI does no wording logic.

## 4. Template library

### 4.1 Format

`templates/*.template.json` (built-in, read-only) and `<data dir>/templates/<id>/<version>.json` (user). Versioning and immutability match recipes (ADR-0011 §2.4).

```jsonc
{ "schema": "hackriff.template", "schema_version": 1,
  "id": "pocsag", "version": 1, "name": "POCSAG paging", "description": "…",
  "provenance": { "kind": "builtin" },          // builtin | user | discovered {job_id, emitter_id, t}
  "recipe": { "id": "pocsag", "version": 3 },   // or "skeleton": { "slots": {…} } for generic templates
  "free": [ { "path": "nodes[clock].params.symbol_rate_bd",
              "domain": { "enum": { "values": [512, 1200, 2400] } } } ],
  "priors": { "families": { "fsk": 1.0 }, "symbol_rate_bd": [[512, 2400]], "bandwidth_hz": [8e3, 25e3],
              "bursty": true, "bands_hz": [[137e6, 174e6], [420e6, 470e6]] },   // bands rank only
  "evidence_targets": { "S4": { "sync_bits": 32 }, "S5": { "kind": "bch", "width": 10 } },
  "plausibility": [ { "field": "ric", "range": [0, 2097151] } ],
  "output_policy": { "content_class": "restricted-paging", "metadata_keys": ["capcode", "function", "baud", "encoding"] } }
```

- **Built-ins:** one per shipped recipe (`rds`, `pocsag`, `acars`, `adsb`) plus generic skeletons: `generic-fsk-framed`, `generic-ook-pwm`, `generic-ook-manchester`, `generic-msk`, `generic-ppm`.
- `priors` is a superset of recipe `match` (ADR-0011 §2.2), read the same way: against **measured** parameters. `bands_hz` only raises rank for an emitter already detected there. Nothing tunes to it.
- `plausibility` and `evidence_targets` contribute S6 ranking evidence only when met by measured frames. A template can never confirm a signal on its own.
- **`output_policy` is the recipe's typed map** (C4): `Template.output_policy` is `hk_recipe::OutputPolicy`, the same object §4.3 clamps (see `recipes/pocsag.recipe.json`). The `metadata_keys` list in the example above is shorthand for that map, not a second schema.

### 4.2 Seeding and pruning from M3 (MAUTO side of ADR-0016)

MAUTO reads, and does not define, these fields of ADR-0016's types. If ADR-0016 names them differently, one adapter in `hk-synth::seed` absorbs the difference.
- `Classification`: `families{name: posterior}` including `unknown`, `open_set_score`, `taxonomy@version`, provenance.
- `SignatureMatch`: `kind (full | partial | none)`, `candidates[{signature_id, version, score, per_field, pipeline_binding}]`, `cluster_id`.

The prior for hypothesis h is:

`π(h) = P_class(family(h)) × match(h | measured params) × band_factor(h)`, with `band_factor` ∈ [1, 1.5].

**Rules:**
- **Open-search floor.** Open skeletons always get ≥ max(P(unknown), 0.2) of the evaluation budget. Priors never starve unknowns.
- **Defer, don't delete — and only the likelihood defers** (C2, per ADR-0016 §8 / T-215). A family is deferred exactly when ADR-0016's `Hypothesis::prune` says so, which reads the **likelihood**, never the prior: a posterior below 0.02 reorders a family (it runs later) but never defers it on its own. Deferred families run after higher-ranked hypotheses, if budget remains, and evidence found under one ranks exactly like any other. (ADR-0021's `deferred_prior` detail still reports the posterior.)
- **Signature fast path:**
  - a `full` match with a pipeline binding is tried first, at the signature's parameter values;
  - a `partial` match narrows its template's free ranges to the signature tolerances;
  - a known `cluster_id` warm-starts the beam from that cluster's last attached result (§5.5).
- **Feedback (writes, through M3 APIs only).** A solved result emits a decode label `{source: decode, family, template, job_id}` to C15 (a CRC-valid decode overrides *automatic* classification, C15 card — but **never a user label**: U3 = A, §5.5), proposes a C18 `Signature` from the solved parameters (provenance `decoder-confirmed`, T-201 route), and feeds its hold-out snippets to T-205's labelled-capture path.

### 4.3 Save as template

`POST /api/analyze/{id}/results/{rank}/template {id, name, description?}` writes a user template with `provenance.discovered`:
- solved stages become fixed parameters, or narrow ranges at twice their measured uncertainty;
- unsolved stages stay free with the job's ranges;
- `output_policy` is clamped to the job's source class.

The result's `recipe` can also be saved through the existing `POST /api/recipes`. Discovered templates rank like any other and confirm nothing.

## 5. Region-analyze API (replaces the T-190 stub)

### 5.1 Routes

| Method | Path | Body / query | Answers |
|---|---|---|---|
| POST | `/api/analyze` | T-190's target, unchanged: exactly one of `selection_id`, `emitter_id`, or `f_lo`+`f_hi` (+ `t_lo?`/`t_hi?`). Optional: `profile` (`quick`\|`standard`\|`deep`), `max_wall_s`, `source` (`auto`\|`ring`\|`live`), `live_s` (≤ 30), `templates` `{only?: [id], exclude?: [id], off?: bool}`, `attach` (default true) | `202 {job}` + `Location: /api/analyze/{id}`; audited `analyze_start` |
| GET | `/api/analyze` | `?state=` | `{jobs: [AnalyzeJob]}`, newest first (the last 50 finished are kept in memory; results persist via attach) |
| GET | `/api/analyze/{id}` | – | `AnalyzeJob` |
| DELETE | `/api/analyze/{id}` | – | cancels a running or queued job (`{job}` with `state: cancelled`, partial results kept) or forgets a finished one; audited `analyze_cancel` |
| POST | `/api/analyze/{id}/results/{rank}/template` | §4.3 | `201 {template: {id, version}}`; audited `analyze_template_save` |
| GET | `/ws/analyze/{id}` (TCP `analyze/<id>`) | – | messages stream `message_schema: hackriff.analyze/1` |

Starting a result as a live decoder uses the existing `POST /api/pipelines {recipe, target}`. No new route is needed.

**Errors** `{error, code}` (messages never echo values): `400 invalid` (T-190's rules, plus a bad profile, source or window); `404 not_found`; `409 outside_window` (live source, band outside the tuned window, no ring IQ); `410 evicted` (the window left the ring before acquisition); `422 no_iq` (no retained IQ in the window: buffer off, gap, gated); `422 power`; `503 busy`; `503 unavailable` (no ring, audit or store); `401`/`405` as usual.

### 5.2 Job object and stream

`AnalyzeJob`: `{id (a<n>), state (queued | acquiring | searching | refining | validating | throttled | done | cancelled | failed), end_reason, target, profile, window: {source (ring | live), t_lo, t_hi, segments, samples, gaps, bursts}, channel: {center_hz, bandwidth_hz, sample_rate_hz}, budget, used: {wall_s, cpu_s, evaluations, hypotheses, cache_bytes}, seeds: {classification_ref, signature_ref, templates: [{id, version, prior_bits, reason}]}, progress: {stage_max, beam, pruned, deferred}, results: [PipelineResult] (≤ 10 ranked), emitter_id, decodes: {stored, valid}, confirm: {rule, outcome (confirmed | already | insufficient | not-attached), evidence_bits, reason}, content_class, created, started, ended, warnings}`.

Stream records drop, never block; each is an idempotent snapshot, so a dropped record loses nothing `GET` doesn't hold: `progress` (≤ 1/s), `best` (the whole `PipelineResult` whenever the top 3 change), `stage` (a result reaches a new stage), `done` (the final `AnalyzeJob`).

### 5.3 Targets and windows

- **`emitter_id`.** Uses the measured (or refined) centre and bandwidth. A T-191 user band, when set, is used and noted. The window defaults to the emitter's newest appearance still in the ring, else `live`.
- **`selection_id`.** Uses the selection's `f_lo`/`f_hi` and `t_lo`/`t_hi`. Without a time window: `live`.
- **Band.** Ad hoc band and optional time.
- **`source: ring`.** Reads the ADR-0014 ring through a new in-memory read (`IqBufferService::read(range, band) -> [segment chunks with provenance]`, additive beside `export_clip`). Segment boundaries become `DISCONTINUITY`.
- **`source: live`.** Collects `live_s` from a ring reader, as T-070's probe does.
- **`auto`.** Ring if the window is retained, else live.
- **Class.** The job takes the source class (`hk_pipeline::class`). Results' metadata (evidence, parameters, verdicts) always flows. `frames_preview` and stored decode content are gated by the §6 stream gate under the template's `output_policy`, or the source class for open results.

### 5.4 Attach

Every finished job with results attaches to an emitter when `attach` is true:
- for an emitter target, that emitter;
- otherwise, the inventory emitter nearest the solved channel in the window (the `refine::emitter_for_channel` rule);
- if there is none, the job's decodes create a candidate through normal ingestion.

It appends an **`emitter_synthesis`** row (hk-model, append-only like `emitter_refined_tuning`):
- `emitter_id`, `job_id`, `engine@version`, `t`, `verdict`, `stage_reached`;
- `evidence_bits`, `prior_bits`, `template`, `recipe` (the rank-1 document inline, plus its hash), `check` summary;
- provenance `synthesized by output analysis`.

`/api/inventory` rows gain `synthesis` (the latest row, summarised). The emitter's measured values are never overwritten.

### 5.5 Confirm-by-decode (T-078 wiring) and trust

- **Stored decodes.** The rank-1 candidate at `Validate` runs over the hold-out window with `messages` outputs through the T-111 ingestion path.
  - `decoder_id` is `synth:<template id | open>`; `decoder_version` is `<engine version>+<recipe hash>`.
  - Each Decode row carries `provenance: {kind: synthesized, job_id, holdout: true, evidence_bits, hypotheses}` (docs/07 §2.15 delta).
  - Open-search decodes use the structural identity scheme `other:hk-framing`, so the existing identity rule never confirms them.
- **New rule** `ConfirmPolicy.synthesized` (actor `hk-pipeline/confirm-synth@1`) confirms a candidate only when all of these hold:
  1. Hold-out evidence ≥ `min_evidence_bits` (64) after look-elsewhere.
  2. ≥ `min_distinct_valid` (3) distinct valid frames on hold-out.
  3. The check has width ≥ 16, or BCH with ≥ 10 parity bits per codeword over ≥ 8 codewords.
  4. Front-end trust holds: ≤ 50 % suspect detections, no overload during the window.

  The lifecycle reason is backend-rendered, e.g. "decoded by synthesized pipeline `generic-fsk-framed`: 5 distinct CRC-16 frames valid on hold-out, 88 bits".
- **Trust rules.** Partial verdicts never change lifecycle state. The rule never demotes, and a user delete wins. A template-bound decode that yields a real identity (e.g. `adsb-icao`) is still marked `synthesized`, and `/api/inventory` shows that provenance next to the identity: a successful decode is strong evidence, never an unexplained fact.
- **User authority (U3 = A, user 2026-09-23; also closes ADR-0016 open question 3).** A user label or user-promoted pipeline is rank 0 and **wins** over a CRC-valid decode that disagrees. The contradicting decode is **recorded** (its Decode rows and pipeline stay) and **shown beside the label** in the inventory and output panel; it is outranked, never applied, never discarded. This is `effective_rank_sql!` as it already stands.
- **Single frames** (§6) — U1 = B (user 2026-09-23): one frame auto-confirms **only when its check was template-fixed and ≥ 24 bits**; searched checks keep the multi-frame requirement. ADR-0022 realises this as one inequality, not a predicate (ADR-0022 §4.2): a template-fixed CRC-24 squitter satisfies it, a searched check on one frame cannot. The false-confirm budget is **≤ 1 wrong Confirmed emitter per week unattended** (ADR-0022 §1.1).
- **Superseded by [ADR-0022](0022-false-confirm-budget.md) (T-548).** The 64 bits / 3 frames / width 16 above are guesses on a one-way door; ADR-0022 derives the gate from the user's budget of one wrong Confirmed emitter per unattended week — **24 analytic hold-out bits**, payable only in analytic-null bits each net of *its own stage's* look-elsewhere, a frame count that is a formula rather than a constant, a width floor of 8 with a 16-bit hard check floor, and the searched-generator discount that [ADR-0021](0021-search-trace-and-negative-result.md) §7A.5 deferred here. T-575 applies it to this section and to §11.5.

## 6. Burst and one-off path

- **When.** The target emitter's sightings are T-075 burst detections, or the window is ≤ 50 ms, or `source: ring` names a past instant.
- **Burst set.** The engine collects bursts in the window that belong to the target: same emitter, same C18 `cluster_id` when M3 provides one, or burst boxes overlapping the band.
  - Each burst is `[t_start − guard, t_end + guard]` (guard = max(2 ms, the burst duration)), read from the ring.
  - Bursts are concatenated with `DISCONTINUITY` between them, capped at 64 bursts or 2 s of IQ.
  - Evidence `n` accumulates across bursts, and hold-out is the odd bursts.
- **Pin on analyze.** At job start the acquired ranges are exported as a pinned clip (`Recording` kind `iq-snippet`, trigger `analyze`) before the search, so ring eviction can't race the job. The clip id is in `window`, and a re-run can target it.
- **Search changes.** The S0 seed is the burst's short-FFT centre (±1.2 kHz, T-075); there is no tracking refinement across time; S2 seeds come from the preamble (assist `periods`); `ppm_demod` skeletons are tried first for ms-scale bursts at ≥ 1 Msps.
- **One burst.** It can reach `solved` (e.g. one ADS-B frame passing CRC-24 under the `adsb` template). It attaches, and confirms on its own only through §5.5's single-frame rule (U1 = B, via ADR-0022's inequality): template-fixed and ≥ 24 bits.

## 7. Evaluation (blind; a priori)

**Protocol.** Fixtures are replayed **through the mock SDR**; truth (`hackriff:truth` annotations) is loaded only by the assert harness. Targets come from blind detection (an inventory emitter or burst detection the run found), never a truth frequency, and jobs start via HTTP `POST /api/analyze`. Each fixture runs **templates on** (the product) and **templates off** (`templates.off: true`, open search, which measures real synthesis). The suite is `acceptance_mauto`, its own `just` step. Every result is reported with SNR, CFO, rate, profile, wall and CPU on a Mac release build; the Jetson re-run comes later.

| Protocol / fixture | Templates on (must) | Templates off (must) |
|---|---|---|
| RDS, `fm_100p8M` (real) | `solved`, rank 1 `rds`, PI/PS equal the `hk_demod::rds` oracle, confirmed, `standard` | `framed` or better: 26-bit linear-block period and offset words found |
| POCSAG, T-095 synthetic 4-channel net (per channel) | `solved`, ≥ 95 % of truth pages on hold-out, confirmed | `solved`: sync 0x7CD215D8 + BCH(31,21) recovered |
| ACARS, T-098 synthetic | `solved`, CRC valid on hold-out | `clocked` or better (MSK + NRZI-encode is a deep structure) |
| ADS-B, SIGNAL-001 squitter scene (burst path) | ≥ 14/16 single squitters `solved`; burst set confirmed | burst set: CRC-24 recovered from ≥ 8 frames, `solved` |
| Generic FSK/OOK sweep (synthetic: 300 Bd–50 kBd, random 16–32-bit sync, RevEng-catalogue CRC-8/16, SNR 6/10/20 dB, CFO ± 0.2 × bandwidth) | n/a | solved ≥ 80 % at 20 dB, ≥ 60 % at 10 dB; ≥ 50 % at least `framed` at 6 dB |
| 915 MHz FSK sensor (T-078 scene) | n/a (no template) | `solved` or `checked`; symbol rate within 1 % |

**Negatives.** Noise, CW, analog FM voice and synthetic OFDM (no blocks) give 0 `solved`, 0 confirmations and 0 created emitters; 200 noise windows at `deep` give 0 `solved`.

**Partial quality for unknowns** (a random-polynomial CRC not in the catalogue, or no CRC): the verdict equals the truth's deepest achievable stage in ≥ 80 % of cases and is **above** truth (over-claim) in ≤ 1 %; whenever the verdict is ≥ `framed`, the symbol rate is within 1 % and the sync word exact.

All thresholds are fixed **before** implementation. The M-12 review may tighten them, never loosen them after seeing results. They are initial guesses, **unverified**.

## 8. Non-goals

- **Channel hopping and trunking.** Synthesis works on one channel; `follow_hops` recipes stay hand-started. That is M4.
- **Encrypted or proprietary payloads.** No key search or cryptanalysis. A high-entropy payload behind a valid frame is characterised ("framed, CRC-16 valid, payload entropy 7.9 bits/byte: probably encrypted or compressed") and left there.
- **OFDM, DSSS, CSS and QAM structures** (no blocks). The classification's `unsupported-structure` is reported as the verdict reason.
- **Automatic analysis of every detection.** Not every detection, and not at every profile: per U2 (§3.3) the attention scheduler auto-queues unexplained candidates at `quick` only, on mains, one at a time; `standard`/`deep` stay user- or API-triggered.
- **ML on the critical path** (§3.3 slot only).
- **Transmit or active probing.**

## 9. Crate placement

| Crate | Holds | Depends on |
|---|---|---|
| `hk-synth` (new) | re-exports the evidence vocabulary from `hk_model::synth` (so `hk_synth::Stage` still resolves); `Evidence` scoring and nulls, candidate and skeleton types, search engine, proposal adapters, `EvidenceObjective`, template schema, loader and seeding | hk-recipe, hk-blocks, hk-estimate, hk-demod, hk-model |
| `hk-blocks` | `Block::evidence` (returning `hk_model::synth::EvidenceSet`), per-block evidence, the batch `run_window` driver | (existing) |
| `hk-pipeline::synth` | job manager, acquisition (ring/live/burst set, pin clip), `synth` chain kind, attach, the `ConfirmPolicy.synthesized` rule | + hk-synth |
| `hk-store` | `IqBufferService::read` backing (ADR-0014 format, read-only) | (existing) |
| `hk-model` | **the evidence vocabulary `hk_model::synth`** (`Stage`, `MetricId`, `GroupId`, `Evidence`, `EvidenceSet`), `emitter_synthesis` table, Decode provenance, template store paths | (existing) |
| `hk-api` (`analyze.rs`) | the routes in §5 | (existing) |

**Why the vocabulary is in `hk-model`** (D1, found by T-848): `hk-blocks` must name `Evidence`/`EvidenceSet`/`Stage` for `Block::evidence`, and `hk-synth` depends on `hk-blocks`, so placing them in `hk-synth` as first written was a dependency cycle. Same pattern as ADR-0016's types in `hk_model::classify`.

**Deltas to write when MAUTO is scheduled:** docs/07 §2.11 (`synthesis`), §2.15 (Decode `provenance`) and a new Template object; docs/api.md "Analyze" (replaces the T-190 section); stream-contract `hackriff.analyze/1`; recipe `schema_version` 3 (`refine.objective.evidence`; **done: T-858**, ADR-0011 §2.4).

## 10. MAUTO task graph (sketch; ids TBD; not in tasks.yaml)

Scheduled only after the M3 exit (T-206). There are no Fable tasks; core-interface tasks go to Opus and are reviewed before merge.

| Id | Task | Deps | Model | Group |
|---|---|---|---|---|
| M-1 | `hk-synth` scaffold: Stage/Evidence/candidate/skeleton/template types, pre-added modules and stubs, ADR-0015 → ACCEPTED review | T-206, ADR-0016 accepted — **waived at acceptance** (§16.5): hk-synth consumes only ADR-0016 §8's landed `SearchSeed` | Opus (core) | SYN-0 — **done: T-848; accepted 2026-09-23** |
| M-2 | `Block::evidence` + evidence for S0–S6 blocks, analytic nulls, calibration tables + py generator, `run_window` | M-1 | Opus (core) | SYN-B |
| M-3 | Search engine: beam, memoisation, pruning, budget/profiles, stop rules, power/throttle | M-1 | Opus | SYN-E |
| M-4 | Proposal operators over assist (sync/codes/fields) with budget charging | M-1 | Sonnet (reviewed) | SYN-P |
| M-5 | Template library: schema validation, loader, built-ins from recipes + generic skeletons, seeding from Classification/SignatureMatch | M-1, T-199, T-201 | Sonnet (reviewed) | SYN-T |
| M-6 | Ring read API + burst-set acquisition + pin-on-analyze clip | M-1, T-178 | Opus | SYN-R |
| M-7 | `EvidenceObjective` over `RefinementLoop`; recipe schema v3 `refine.objective.evidence` | M-2, M-3 | Opus (core) | SYN-E (after M-3) |
| M-8 | `/api/analyze` jobs, stream, cancel, audit; docs/api.md + api_contract tests (replaces T-190's 501) | M-3, M-6, T-190 | Opus (core) | API |
| M-9 | Attach (`emitter_synthesis`), synthesized decode provenance, `ConfirmPolicy.synthesized` with hold-out | M-8 | Opus (core) | W |
| M-10 | Save-as-template; label → C15, signature proposal → C18, snippets → T-205 | M-5, M-9, T-201, T-205 | Sonnet | SYN-T |
| M-11 | MUI: Analyze progress, ranked results, evidence ladder, "start as pipeline" and "save template" in the output panel | M-8, T-192, T-195 | Sonnet | MUI-X1 |
| M-12 | `acceptance_mauto` blind suite (§7), generic-FSK synthetic generator in `py/` | M-4, M-5, M-7, M-9 | Opus | M-E |
| M-13 | Jetson budget/power measurement and profile re-baseline (interactive, needs the Jetson) | M-12 | Opus | JETSON |
| M-14 | **Required** (U4 = A, user 2026-09-23) `psk_demod` block (ADR-0011 catalogue addition). **The block is delivered by T-609** (superseding this row's narrower "optional Costas" scope; T-610 `viterbi` + T-611 `reed_solomon` complete the CCSDS path). **Not covered by T-609, and owed before PSK reaches bits in a search:** `psk_demod`'s `Block::evidence()` S1 metric (EVM/lock) and its calibration table — M-2's contract, which must include `psk_demod`; a linear-modulation (PSK) S1 alternative in the open skeletons and seeding (M-3/M-5; §4.1's built-in skeleton list has none); and a burst/preamble-driven acquisition mode (T-609 needs a 512–1024-symbol window, so short PSK bursts lose their start) — **delivered by T-875** (`burst: true`, feed-forward acquisition over each burst's own samples; ADR-0011 §9 notes). | M-2 | Opus | SYN-B |

**Waves** (≤ 4 Rust builders at once): (1) M-1; (2) M-2, M-3, M-4, M-6, with M-5 once T-199/T-201 are done; (3) M-7, M-8, M-5; (4) M-9, M-10, M-11; (5) M-12, then M-13.

**Amended by [ADR-0021](0021-search-trace-and-negative-result.md) (T-549/T-550).** Nothing in this table owned the **search trace** — M-3 prunes, M-8 serves, M-11 renders, and the record of what the engine rejected and why was produced by nobody — and nothing owned the **negative result**, so `energy` because the beam never left S0 read identically to `energy` because every S1 family measured below its floor. ADR-0021 gives the trace to **M-3** (inside the beam: a beam that has discarded the information cannot have it retrofitted, only re-instrumented) and widens M-1, M-8, M-9, M-11 and M-12; see its §12 for the amended rows, and §11.3 for the changes it makes to §§1.3, 3.4, 5.1–5.5, 7, 8 and 11.1 above.

## Options considered

- **Brute-force grid over recipes.** Rejected: it is intractable (docs/15 §1), and it can't use proposal operators for sync words or polynomials.
- **Evidence as diagnostic ports.** Rejected: per-item buffers and taps for a per-window scalar, and nothing to normalise against.
- **One weighted sum of raw metrics** (dB, rates, openness). Rejected: incomparable units, and weights tuned per fixture overfit. Significance bits have a null and a unit.
- **Priors inside the evidence score.** Rejected: a strong template prior could "solve" a weak decode, breaking blind-first. They stay separate numbers.
- **MCTS or genetic search.** Deferred: beam search with memoisation is deterministic, testable and budget-predictable. `NodeHeuristic` leaves room.
- **Synthesis as a plugin process.** Rejected: the search drives hk-blocks directly, and a process boundary buys nothing here (no GPL code).
- **A separate "synthesized decoder" artifact type.** Rejected: results are recipes, so the workbench, hot edit, captures and the inspector apply unchanged.

## Consequences

- Unknown signals get a systematic path from energy to framed data. Every result is a runnable recipe with an evidence ladder the UI shows verbatim.
- Confirmation from synthesis needs hold-out evidence, which is stronger than the existing one-valid-decode identity rule for hand-started recipes. That asymmetry is deliberate: search inflates chance fits.
- Costs: calibration tables per block version, an additive `evidence()` on the block contract, recipe schema v3, and a new chain kind competing for CPU on a battery device.

## Open questions (for the user)

**All fifteen of the questions below — these five, §11.10's five and §12.12's five — are consolidated, costed and given a recommendation in [docs/20, the MAUTO decision brief](../20-mauto-decision-brief.md) (T-553). Five survive as questions for the user; the rest are decided or sent to measurement there. Answer them from that table, not from these lists.**

**Answered 2026-09-23 (user, from docs/20's table):** Q1 → U1 = B; Q2 → budgets sent to measurement (T-552), battery half → U2 = B; Q3 → U2 = B; Q4 → docs/20 D1 (local JSON); Q5 → U4 = A. Kept below as the record of what was asked.

1. **Single-burst confirmation.** Should one frame passing a template-fixed ≥ 24-bit check (e.g. an ADS-B squitter) auto-confirm, or only attach, leaving promotion to you (proposed)?
2. **Budget defaults and battery.** Are `quick` 3 s / `standard` 20 s / `deep` 120 s right for a handheld? Should `deep` be refused on battery?
3. **Auto-analyze.** Should the attention scheduler eventually queue analyze jobs for unknown candidates by itself, or stay user-triggered only?
4. **Discovered templates.** Keep them as local JSON files (proposed; exportable), or plan a share/export format now?
5. **Generic PSK.** Include the `psk_demod`/Costas block (M-14) in MAUTO, or accept that PSK stops at `demodulated`?

## 11. Amendment — a candidate **is** a decode pipeline (T-220, 2026-09-15, from the user)

**Status:** PROVISIONAL, planning only, no code. Source: [docs/15 §10](../15-decoder-synthesis.md), written by the user after live testing. §§1–10 stand unchanged; this amendment changes *where the search's results live* and *what an inventory entry contains*. Task ids CP-1+ are proposals for the coordinator, not entries in `tasks.yaml`.

**The change in one line.** An Emitter stops being "a box with one family label" and becomes **an emission plus the competing decode hypotheses for it**, each hypothesis a runnable pipeline carrying its own evidence. Confirm-by-decode promotes the winner. §5.4's attach ("append an `emitter_synthesis` row") becomes one producer among several of the same object.

### 11.1 The object: `CandidatePipeline`

```jsonc
{ "id": "cp_<uuidv7>", "emitter_id": "…",
  "recipe": { "id": "rds", "version": 3, "hash": "…", "body": null },  // body inline for a synthesized prefix never saved to disk
  "channel": { "center_hz": 100.8e6, "bandwidth_hz": 200e3, "sample_rate_hz": 240000 },
  "stage_reached": "S5", "verdict": "solved",              // §3.4 vocabulary, unchanged
  "evidence": { "bits": 88.0, "holdout_bits": 74.0, "prior_bits": -1.2,
                "stages": [ { "stage": "S2", "node": "clock", "metric": "eye_open", "raw": 0.81, "n": 4096, "bits": 11.0, "quality": 0.75 } ],
                "check": { "kind": "crc", "model": "CRC-16/IBM", "width": 16, "distinct_valid": 5, "corrected_excluded": 2 } },
  "status": "proposed",            // proposed | running | superseded | promoted | rejected
  "origin": "synthesis",           // detection | track | classification | template | synthesis | user | listen
  "output_kind": "messages",       // messages | audio | inspector | none
  "provenance": { "engine": "hk-synth@1", "job_id": "a17", "actor": "hk-pipeline/confirm-synth@1", "t": "…" },
  "links": { "supersedes": [], "superseded_by": null, "pipeline_instance_id": null } }
```

- **A pipeline is a recipe, always.** Every row's `recipe` satisfies `Recipe::validate` (ADR-0011 §2.2) — a full document or a valid prefix (§1.2). Recipes live on **disk**, not in SQLite (`crates/hk-pipeline/src/recipes/store.rs`: built-ins `recipes/<id>.recipe.json`, user versions `<data dir>/recipes/<id>/<version>.json`, immutable, save = latest + 1), so the row stores `(id, version, hash)` for a saved recipe and an inline `body` for a synthesized prefix that was never saved. "Start this candidate" is the existing `RecipeRuntime::start(recipe, Target::Emitter)` behind `POST /api/pipelines`, with the instance id written back to `links.pipeline_instance_id`.
- **Relation to Emitter.** One Emitter owns 0..N pipelines. The emitter's **measured** `f`/`BW` are never overwritten by a pipeline's `channel`: a pipeline may sit offset in the skirt and still decode (the observed FM case), and that offset is the hypothesis, not a correction to the measurement. The T-191 user band is unchanged.
- **Relation to Classification (ADR-0016).** Two ladders, one displayed answer, no second arbitration: **Classification decides the displayed family; the top-ranked pipeline decides the displayed decode/output.** A pipeline never writes a family directly. Promotion emits an ordinary `ArbRank::Decoder` (rank 1), `Stage::Decoder` row through `Repository::record_classification` (`repo/classify.rs:168`), so a promoted pipeline wins the family *through* the existing rank (`effective_rank_sql!`, `repo/classify.rs:18`), not around it. An unpromoted or partial pipeline writes nothing to Classification — failing to decode is not evidence against a modulation (ADR-0016 §8). *(Honest note: `record_classification` has no production caller yet; today's families still arrive via `insert_classification_from`.)*
- **Relation to Decode.** `decode` gains a nullable `candidate_pipeline_id`, so every decoded record is attributable to the hypothesis that produced it and a superseded hypothesis's records stay readable.
- **Relation to Detection/Track/Emission.** Unchanged — the immutable measurement. A pipeline is interpretation: versioned, reversible (docs/07's first rule).
- **Zero rows is legal** and is exactly today's behaviour: an emitter with no pipeline reads as an implicit `energy` hypothesis (S0, verdict `energy`). Rows are **materialised lazily** — on analyze, on Listen, on a user start, or as soon as a second hypothesis exists — so ordinary detection does not create a row per box. (Open question 1.)

### 11.2 Where pipelines come from

| Origin | Trigger | Initial content |
|---|---|---|
| `detection` / `track` | first materialisation of an emitter that has no pipeline | bare `energy@1` channel recipe, evidence = S0 only |
| `classification` | M3 family + `SearchSeed` (ADR-0016 §8), on demand | one template-derived prefix per plausible family, `prior_bits` set, `evidence.bits` = 0 |
| `template` | `GET /api/recipes/match`, or a `full`/`partial` SignatureMatch with a recipe binding | the bound recipe at the signature's parameter values |
| `synthesis` | an `/api/analyze` job (§5.4) | each `PipelineResult` of rank ≤ 3, with its full stage ladder |
| `user` | starting a recipe against the emitter, or hand-authoring one in the workbench | `status: running`; evidence filled from the live status ladder (§2.1 mirror) |
| `listen` | the Listen action (T-221) | an audio-sink pipeline, `output_kind: audio` |

Priors never create evidence: a `classification`- or `template`-origin row starts at zero bits and ranks last until it is run.

### 11.3 Ranking, supersession, reversibility

- **Rank key:** `(status_rank, stage_reached, holdout_bits, evidence_bits, −created)`, `status_rank` = promoted 0 > running 1 > proposed 2 > superseded 3 > rejected 4. **`prior_bits` is reported and never ranks** — §1.3's rule, carried into the inventory.
- **Same-hypothesis test** (only same-hypothesis rows may supersede): same emitter, channel overlap ≥ 0.6 of the narrower bandwidth, and the same structural identity (recipe id, or skeleton + slot `choices`). Two *different* structures on one band (a WFM hypothesis and an FSK hypothesis) coexist as competitors and never supersede; they resolve only by promotion.
- **Supersession margin:** A supersedes B when both are same-hypothesis and A's `holdout_bits` exceeds B's by ≥ 4 bits (16:1). Inside the margin both stay visible as ranked alternatives — "three competing WFM pipelines for one station" is a legal displayed state, not a bug.
- **Reversible by construction.** `status` is a cached projection of an append-only `candidate_pipeline_event` log (`created`, `evidence`, `superseded`, `revived`, `promoted`, `unpromoted`, `rejected`, `user_*`), mirroring how `emitter_lifecycle` backs `emitter.lifecycle_state` (`repo/lifecycle.rs:172`). New evidence **revives** a superseded row rather than resurrecting deleted state. Promotion marks losers `superseded`, never `rejected`, so revoking a promotion restores the field. Nothing is deleted; only a user deletes.
- **User authority** is rank 0 as in ADR-0016: a user promote/reject wins, is audited, and is not overridden by evidence — the contradicting evidence is still recorded and shown.

### 11.4 Overlap resolution (the T-219 rules) in this model

Three problems, one mechanism — **competition between hypotheses over a band** — differing only in the kind of evidence that binds them:

1. **Duplicate boxes for one emission** (today's FM case: several offset candidates, each decoding adequately). End state: **one Emitter, N competing pipelines at different centres**, strongest first. `same_emission_score` / `same_emission_partners` / `merge_same_emission_rows` (`repo/cluster.rs:440/497/544`) already express half of this; what is missing is somewhere for the losers to live — the pipeline row.
2. **A Confirmed signal suppresses overlapping candidates.** A new candidate contained in a Confirmed emitter's band with no distinguishing evidence is not a new emitter: it attaches to that emitter as another hypothesis (or, before the object exists, is suppressed with a recorded reason and a link). Suppression is append-only and reversible; raw detections, tracks and history are always kept.
3. **Receiver artifacts are same-source duplicates** (user, 2026-09-15). A detection can be an artifact of another signal at a *deterministic, predictable* frequency: an **image** at `2·f_LO − f`, a **harmonic** at `n·f`, or **intermodulation** at `a·f1 ± b·f2` from strong confirmed emitters. Such a candidate is attributed to its source (`artifact_of {emitter_id, kind: image|harmonic|intermod, order, predicted_hz, error_hz}`) rather than treated as independent, and is hidden from the top-level inventory while its rows are kept. This is a *geometric* test — frequency arithmetic, relative level, co-onset/co-offset with the source — so it needs no decode and reuses the existing SpurMask / `image_candidate` / `suspect_imd` machinery (docs/07 §2.9).
   - **Distinct from multipath** (T-222): multipath duplicates carry the **same decoded content**, a content-correlation test that requires a decode. Both land in the same "same-source duplicate" branch of the model, tagged `evidence_kind: geometric` versus `content`.

**Guard case — two genuinely distinct adjacent stations must not merge.** Any *distinguishing* evidence blocks a merge, in this order: two different decoded identities (e.g. different RDS PI) always block — this is already `same_emission_score`'s "not both identified" gate; a `Fingerprint::compare` distance beyond `Tolerances` blocks (`crates/hk-model/src/cluster.rs:199/498`); non-overlapping −3 dB extents with centres separated by more than the summed measurement uncertainty blocks. Overlap alone is never sufficient to merge — only sufficient to make two rows *compete*.

**What T-219 can build now** (no `candidate_pipeline` table, no evidence bits — `Block::evidence` is MAUTO work):
- Confirmed-suppresses-overlapping, as an append-only suppression/link row with a reason, reversible, raw data kept (`emitter_link` is the existing precedent).
- The extended overlap + guard rules on `same_emission_score` / `fingerprint_candidates` (`repo/cluster.rs:440/731`).
- `duplicate_of` and `artifact_of` links, with the geometric image/harmonic/intermod predictor over confirmed strong emitters, as a **flag only** — never a delete.
- A **duplicate group** ranked by a provisional proxy (SNR × duty × trust, explicitly *not* the bits ladder), so the strongest box is the one shown.

**What needs the full model:** evidence in bits, promotion by decode, competing pipelines as first-class displayed alternatives, content-level multipath (T-222).

**The constraint on T-219 so it doesn't fight this:** record grouping as **append-only rows keyed by emitter, carrying reason and score** — never by mutating or deleting the losing rows. A T-219 duplicate group then becomes, unchanged, the set of competing pipelines on one emitter (CP-6).

### 11.5 Confirm-by-decode via promotion

- **Promotion is the lifecycle event.** `status → promoted` triggers the §5.5 `ConfirmPolicy.synthesized` check; if it passes and the emitter is a `candidate`, `change_emitter_lifecycle` (`repo/lifecycle.rs:172`) confirms it with a backend-rendered reason naming the pipeline. Thresholds unchanged: **hold-out evidence ≥ 64 bits after look-elsewhere, ≥ 3 distinct valid frames, check width ≥ 16** (or BCH ≥ 10 parity bits over ≥ 8 codewords), plus front-end trust.
- **Corrected frames are never CRC-valid evidence (T-210).** An invariant on the evidence object: `check.distinct_valid` counts only frames valid **without** FEC correction. Corrected groups are counted separately as `corrected_excluded` for display and contribute **0 bits**. A pipeline whose only "valid" frames were corrected can never reach `solved`.
- **One promoted row per `output_kind` per emitter.** A promoted `messages` pipeline and a promoted `audio` pipeline coexist (decode and Listen at once); promoting a second `messages` pipeline supersedes the first.
- **No rule demotes** (docs/07 §2.11; `change_emitter_lifecycle` forbids a return to `candidate`). Unpromoting a pipeline leaves the emitter confirmed; only a user deletes an entry.
- Single-burst behaviour is unchanged from §5.5: `verdict: solved` attaches and does not auto-confirm (§10 open question 1 still stands).

### 11.6 Migration sketch

- **Migration `0008_candidate_pipeline.sql`** (0007 is M3's classification migration; `MIGRATIONS` in `repo/mod.rs:117`), all additive:
  - `candidate_pipeline` — the §11.1 row; `recipe_id`/`recipe_version`/`recipe_hash` plus nullable inline `recipe_body`; `status` as a cached projection.
  - `candidate_pipeline_event` — append-only with a no-update trigger (the `emitter_lifecycle` pattern); the source of truth for `status` and reversibility.
  - `candidate_pipeline_evidence` — append-only evidence snapshots (stage ladder as JSON), so re-evaluation appends rather than overwrites.
  - `decode.candidate_pipeline_id` — nullable column, attribution only.
  - The T-219 link/suppression rows (landed earlier) gain a nullable `candidate_pipeline_id` so they fold into the object.
- **Existing rows are untouched and not backfilled.** An emitter with no `candidate_pipeline` row reads exactly as today. `emitter_classification` is unchanged — its append-only trigger, `effective_rank_sql!`, `FAMILY_ORDER` and every inventory reader keep working, because promotion writes an ordinary rank-1 decoder row rather than a new kind of family evidence. `emitter_synthesis` (§5.4) stays the job-level audit row and gains a pointer to the pipelines its job created.
- **Staging, so nothing breaks in M0/M1/M2 acceptance:**
  1. **T-219 (M3, now):** links, suppression, duplicate groups and artifact flags only; blind acceptance must not regress, and the adjacent-station guard is a new test.
  2. **MAUTO wave 0 (CP-1):** tables and read path land **inert** — no writer, no behaviour change, nothing reads them for a decision.
  3. **CP-3/CP-4:** synthesis attaches pipelines; promotion drives `ConfirmPolicy`.
  4. **CP-6:** T-219's groups migrate onto the object (append-only row migration, no semantics change).
  5. **T-221/CP-7:** Listen becomes an audio-output pipeline. Listen is today its own chain type (`hk-pipeline/src/chains/listen.rs`, `ChainKind::Listen`) served through the generic on-demand opener (`OpenerRegistry::with("listen", …)`, `/ws/open/listen`), with **no** listen-specific hk-api route — so the opener name keeps working unchanged while the implementation moves onto a recipe.
  6. **CP-8:** the UI shows competitors and evidence ladders.
- **Reversal cost** is low at every step: 2–4 are additive tables plus one nullable column; dropping the feature means ignoring the rows.

### 11.7 docs/07 delta (sketch, applied by the implementing task)

- **§2.11 Emitter:** an Emitter owns 0..N **candidate decode pipelines** (the new CandidatePipeline section below), ranked by evidence; its measured `f`/`BW` are never overwritten by a pipeline's channel; confirm-by-decode promotes a pipeline and confirms the emitter. Add the `duplicate_of` / `artifact_of` / `suppressed_by` links from §11.4.
- **New CandidatePipeline section — the next free docs/07 number (§2.33 today; §2.28 is `TrunkSystem`, T-266 — C6):** the §11.1 object — identity, append-only event lifecycle, ranking and supersession, relation to Recipe/Classification/Decode, retention (kept with the emitter; superseded rows summarised, never deleted), and tests.
- **§2.15 Decode:** `candidate_pipeline_id`, and the T-210 rule that corrected frames are not CRC-valid evidence.
- **§2.21 Classification:** one clarifying sentence — pipelines carry evidence, Classification carries family; a promoted pipeline writes a rank-1 `decoder` row and wins the family through the existing ladder, not around it.

### 11.8 API deltas (listed, not implemented)

| Method | Path | Delta |
|---|---|---|
| GET | `/api/inventory` | row gains `pipelines {count, top: {id, recipe, verdict, evidence_bits, status, output_kind}}` beside `classification` (`hk-api/src/query.rs:772`); `duplicate_of` / `artifact_of` / `suppressed_by` when applicable; filters `?duplicates=hidden\|all`, `?artifacts=hidden\|all` |
| GET | `/api/inventory/{id}/pipelines` | ranked competing hypotheses with evidence ladders and supersession history |
| POST | `/api/inventory/{id}/pipelines` | create a hypothesis from a recipe or template (user), audited |
| POST | `/api/inventory/{id}/pipelines/{pid}/promote` \| `/reject` \| `/start` | promotion drives §11.5; `start` is the existing `POST /api/pipelines` with a back-link; all audited |
| GET | `/api/inventory/{id}/decode` | gains `candidate_pipeline_id` |
| GET | `/api/analyze/{id}` | each result gains `candidate_pipeline_id`; attach creates the rows |
| — | stream `candidates` | ADR-0004 `messages`, metadata only: one record per change of top-ranked pipeline, status or suppression |

`docs/api.md` and `crates/hk-cli/tests/api_contract.rs` move together (T-079 rule).

### 11.9 Task graph (proposed ids CP-1+; the coordinator applies them)

| Id | Task | Deps | Model | Group |
|---|---|---|---|---|
| CP-1 | `hk-model` candidate-pipeline types + migration 0008 + repo (append-only events, rank function), landed **inert** | T-219, this amendment reviewed | Opus, core_interface | CP-0 |
| CP-2 | Ranking + supersession engine: same-hypothesis test, 4-bit margin, revive-on-new-evidence; property tests for reversibility | CP-1 | Opus | CP-R |
| CP-3 | Synthesis attach writes pipelines (replaces §5.4 attach-only); `decode.candidate_pipeline_id` | CP-1, M-8, M-9 | Opus, core_interface | CP-W |
| CP-4 | Promotion → `ConfirmPolicy` wiring: hold-out rule, corrected-group exclusion (T-210), one promoted row per `output_kind` | CP-3 | Opus, core_interface | CP-W |
| CP-5 | Routes (§11.8) + `docs/api.md` + contract tests | CP-1 | Opus, core_interface | API |
| CP-6 | Fold T-219's duplicate groups, suppressions and artifact links onto the pipeline object | T-219, CP-2 | Sonnet (Opus review) | CP-R |
| CP-7 | Listen as an audio-output pipeline, per T-221's plan, on this object | T-221, CP-1, CP-5 | Opus | CP-A |
| CP-8 | MUI: competing-pipeline list, evidence ladder, promote/reject, duplicate-group collapse, artifact badge | CP-5 | Sonnet | MUI-X |
| CP-9 | Blind acceptance: one station → one emitter with N ranked pipelines; the adjacent-station guard case; artifact attribution; promotion confirms | CP-4, CP-6 | Opus | CP-E |

**Waves** (≤ 4 Rust builders): (1) CP-1; (2) CP-2, CP-3, CP-5; (3) CP-4, CP-6, CP-7; (4) CP-8, CP-9. No Fable tasks.

### 11.10 Open questions (for the user)

*Consolidated with §10's and §12.12's lists in [docs/20, the MAUTO decision brief](../20-mauto-decision-brief.md) (T-553): Q1, Q2/Q3 and Q4 are decided there as engineering defaults or measurements; only Q5 (merged with ADR-0016 open question 3) goes to the user.*

**Answered 2026-09-23:** Q1 → docs/20 D2 (lazy); Q2 → D3 (a set of output kinds); Q3 → D4 (hidden by default); Q4 → measurement (T-547, still a first guess); **Q5 → U3 = A, user: the user wins (rank 0), the decode is recorded and shown beside the label** (§5.5).

1. **Lazy or eager rows.** Materialise a bare `energy` pipeline only on demand (proposed), or give every emitter one from creation so the inventory is uniform, at the cost of a row per box?
2. **Two promoted pipelines.** Is "one promoted decode plus one promoted audio pipeline per emitter" right, or should exactly one pipeline ever be promoted?
3. **Artifact visibility.** Should image/harmonic/intermod-attributed candidates be hidden by default (proposed), or shown greyed under their source so you can see the front end misbehaving?
4. **Supersession margin.** Is 4 bits (16:1) the right bar for one hypothesis to hide another, or should competitors always stay visible until promotion?
5. **User versus decode.** A user-promoted pipeline versus a CRC-valid decode from a different pipeline — user wins (proposed, matching ADR-0016 rank 0), or the decode wins and the contradiction is flagged? Same call as ADR-0016 open question 3.

*Unverified in this amendment: the 0.6 channel-overlap fraction, the 4-bit supersession margin, the SNR × duty × trust proxy for T-219, and the artifact-detection tolerances — all first guesses, to be measured against the real FM capture and the adjacent-station guard case.*

## 12. Amendment — Listen as an audio pipeline (T-221, 2026-09-15, from the user)

**Status:** PROVISIONAL, planning only, no code. Source: [docs/15 §10](../15-decoder-synthesis.md), written by the user after live testing. Requires [ADR-0011 §8](0011-decoder-workbench-contracts.md) (the audio sink block, the `audio` output kind, the live-edge policy). §§1–11 stand unchanged; this section says how the **existing, live-tested** Listen path becomes one of those pipelines **without ever going dark**, because the user tests live and a stage that breaks Listen is not acceptable.

### 12.1 What Listen is today (the facts the plan is measured against)

`crates/hk-pipeline/src/chains/listen.rs`, 1093 lines:

1. **Gate** — `listen_class` runs *before any ring read*; restricted bands and restricted source classes refused whatever else is true; unclassified content fails closed.
2. **Admission** — `max_listeners` (8) plus a CPU budget, costed at the dearer mode until the mode is known (T-066, T-071).
3. **Probe** — `probe_s` of live IQ; a C13 estimate plus T-012 mode selection (`hk_demod::audio::probe`) choose **mode** ∈ {`wfm`, `nbfm`, `am`, `usb`, `lsb`, `cw`}, centre and bandwidth. The gate runs again on the probed channel.
4. **Refine** — T-070's `RefinementLoop` with `WfmObjective`; writes an append-only `emitter_refined_tuning` row (provenance `refined by output analysis`); `LiveRefiner` re-refines in the background and retunes the demodulator under hysteresis.
5. **Stream** — `AudioDemod` (squelch, AGC) → 20 ms `ri16_le` records at 48 kS/s plus type-3 status records, through a `Publisher` with a small per-consumer queue (drop-not-block, ≈ 0.6 s).

Structurally: **no `/api` route at all.** It is the on-demand opener `listen` (`/ws/open/listen`, TCP `open/listen`), a `ChainKind::Listen`, **per-consumer** (session dropped → producer stops), and a source re-plumb into another class or rate **ends** the chain so the client reconnects.

### 12.2 The decomposition — and the part the framing hides

"Listen is just a decode pipeline with an audio sink" resolves into **three** things, not one:

1. a **chooser** — probe → mode → *which recipe*, with seed parameters;
2. an **audio pipeline** — ADR-0011 §8, the recipe whose sink is `audio_out`;
3. an **opener** — the target-shaped, per-consumer entry point (`listen?emitter=…`) that ties a consumer to (1) + (2).

Only (2) is the recipe. **(1) is what the one-line framing hides.** A recipe is a *fixed* structure, so "the user never picks the mode" has to live outside it. Name it and own it: `hk_pipeline::audio::choose` probes the live edge, then ranks the analog recipes against the **measured** family, bandwidth and features (`GET /api/recipes/match`'s rule, ADR-0011 §2.4, with T-164's zero-weight-on-frequency ranking) and returns `(recipe_id, seed params)` — or `legacy` (§12.5).

That is precisely a depth-1, budget-1 instance of §3's search restricted to the analog skeletons, which is why the unification is real — and also why it only *completes* when MAUTO exists. Until then the chooser is the existing probe, wrapped, with no new estimation and no new thresholds.

### 12.3 Ownership: who stops an audio pipeline

A Listen chain is owned by its consumer; a recipe pipeline is a named object with a lifecycle. Both are needed, and the difference is not cosmetic:

- **Ephemeral** (the opener's mode, today's semantics preserved): `/ws/open/listen?emitter=…` starts a pipeline owned by the session — it stops when the socket closes, it is never saved, and it appears in `GET /api/pipelines` with `owner: "session"` while it runs.
- **Explicit**: `POST /api/pipelines {recipe, target}` with an `audio` output makes an ordinary named pipeline that outlives any consumer. Opening `listen?pipeline=<id>`, or the always-on `audio/<pipeline>/<output>` stream, **attaches**.
- **Attach, don't duplicate**: an opener whose target already has a running audio pipeline attaches to it. Two browser tabs listening to one station must not build two DDCs — today they get two chains, and under the pipeline model that would also be two rows.

### 12.4 The Listen API, surface by surface

**Nothing is deprecated.** The opener stays the compatibility surface **permanently**, because it is what the UI dock, `py/examples/hk_audio_wav.py` and the TCP one-liners already speak, and because "start a pipeline, then open its stream" is a worse interface for the one-click case.

| Surface | Fate |
|---|---|
| `GET /ws/open/listen?emitter=\|detection=\|f_lo=&f_hi=` | **Kept, unchanged, permanently.** Re-implemented as chooser → ephemeral audio pipeline → that pipeline's `audio` output. No parameter added or removed; still **no `mode` parameter**. |
| TCP `open/listen?…` | Kept, identically. |
| `GET /api/streams` → `on_demand[listen]` | Kept: same `kind`, `datatype`, `sample_rate_hz`, `params`. |
| Audio stream header (stream contract §12.2) | **Additive only**: `pipeline_id`, `recipe`, `output_id`, `edit_rev` (ADR-0011 §8.2). Every existing key keeps its name and meaning. |
| Status records (type 3) | Kept key-for-key; per-node metrics added alongside. |
| Refusal codes (`4403/4404/4409/4503`; `403/404/409/503`) | Kept: the pipeline start's refusal maps onto the same codes, and the pre-attach gate still runs before any ring read (ADR-0011 §8.3). |
| `/api/status` `listen.budget` and `budget` | Kept: audio pipelines count as listeners (ADR-0011 §8.8). |
| `POST /api/outputs/record/start {kinds:["audio"]}` | **Unchanged in this plan.** It may later record an audio pipeline's output instead of opening its own chain; not required, not scheduled. |
| `GET /api/analysis/strongest` | Unrelated and unchanged (it picks a *target*, not audio). |
| **New, additive** | `POST /api/pipelines` accepts an audio recipe; `GET /api/pipelines/{id}` shows `audio` outputs; `listen?pipeline=<id>` attaches; §11.8's `GET /api/inventory/{id}/pipelines` lists the `listen`-origin row. |

### 12.5 Per-mode cutover (why nothing has to break)

Blocks exist, or are specified by ADR-0011 §8.4, for **WFM / NBFM / AM**. **None exist for USB / LSB / CW**, which Listen serves today. So the chooser returns either a recipe **or** `legacy`, and `legacy` runs today's chain unchanged. The migration flips modes one at a time as each recipe proves parity, and SSB/CW may legitimately stay `legacy` **forever** — that is an acceptable end state, not a failure. **Decided (U4 = A, user 2026-09-23):** SSB and CW stay `legacy` permanently; no `ssb_demod`/`cw_demod` is funded, and `chains/listen.rs` stays in the build for those two modes (LP-8 retires the chain for cut-over modes only; docs/20 D6).

### 12.6 Retune and refinement

- **Ownership of the closed loop does not change.** It stays `hk_pipeline`'s refinement loop over T-070's `RefinementLoop`, with the `wfm-pilot` builtin objective (ADR-0011 §8.7). What changes is only that the objective is **declared by the recipe** instead of hard-coded in the chain — and that the very same loop already serves digital recipes (`crc.error_rate → min`). Background re-refinement, hysteresis, the `emitter_refined_tuning` rows and the header/status `refinement` fields all stay exactly as they are. The emitter's **measured** `f`/`BW` are still never overwritten (§11.1).
- **Applying a refinement** becomes an ordinary hot edit instead of the bespoke `retune_refined`: the loop writes the new centre/bandwidth into the pipeline's `input`, and the runtime re-plumbs the channel at a chunk boundary (ADR-0011 §2.3, "`input` changed"), which is what Listen's in-place retune already does by hand. Gain: refining a *digital* pipeline uses the identical mechanism.
- **Source retune (the radio moves).** Today a re-plumb into another class or rate **ends** the chain and the client reconnects; an in-place retune keeps it. **Keep that observable behaviour through the whole migration.** The UI's `AudioSession` and every TCP client handle "stream ended"; changing it mid-migration would be a live-visible change with no test behind it. Making an audio pipeline *survive* a class-changing retune is a deliberate **non-goal** here and a candidate improvement afterwards (it needs the content-class derivation to re-run mid-pipeline). Open question 1.

### 12.7 One output model, one dock, two panels

The UI already has the right shape but keyed on **two different backend concepts**: `ui/src/app/dock/slice.ts` has `OutputEntry {kind: "audio" | "records"}` where an audio entry is *a socket the page opened* and a records entry is *a pipeline output*; `ui/src/app/explore/output-panel.ts` has `PanelSource = {kind: "digital", pipelineId} | {kind: "audio", outputId}` and `collectPanelSources` reconciles the two lists by hand.

Under this amendment both become `{pipeline_id, output_id, kind}` and the dock lists **a pipeline's outputs**:

- the **panel widget is chosen by output kind**, not by which API produced it: `audio` → waveform scope plus level/squelch/AGC meters; `messages`/`inspector` → the packet inspector (T-154 components, reused by T-195);
- **RDS stops being a special case.** Today the audio panel's PS/RT comes from `/api/inventory/{id}/decode` — a path with no relationship to the Listen socket beside it. With ADR-0011 §8.9's recipe, PS/RT is a `messages` **sibling output of the same pipeline**, so the FM panel renders two outputs of one object. This is the concrete payoff of the whole amendment;
- **outputs are independently subscribable**: the pipeline runs while any output has a consumer or while it is promoted, so "RDS without audio" and "audio without RDS" are both ordinary states rather than special cases;
- `collectPanelSources` then keys on one list instead of reconciling two, and its "a decode output wins over a Listen stream" heuristic becomes §11.3's existing rank over the emitter's pipelines.

### 12.8 Two corrections this amendment forces on §11

1. **`output_kind` must become `output_kinds` (a set).** §11.1 gives a `CandidatePipeline` a single `output_kind`, and §11.5 says "one promoted row per `output_kind` per emitter". ADR-0011 §8.9's FM recipe has **two** outputs (audio + messages), so a single-valued field cannot describe it. Read §11.5's rule as: *no two promoted pipelines on one emitter may claim the same output kind*; a pipeline claiming `{audio, messages}` promotes for both and conflicts with any other promoted pipeline claiming either.
2. **An audio pipeline can never confirm an emitter by itself.** Origin `listen` (§11.2) is produced by the chooser: `origin: "listen"`, `output_kinds: ["audio"]`, evidence from the live status ladder (§2.1's mirror), not from a search. Audio has no check word, so such a pipeline reaches at most `verdict: demodulated` (S1) on §3.4's ladder and can never satisfy §5.5's confirm rule (≥ 16-bit check, ≥ 3 distinct valid frames, ≥ 64 hold-out bits). **Listening to a station must never promote it to Confirmed.** Its *RDS sibling output* may, on its own decode evidence. Worth stating plainly, because this is the one place where "audio is just another decode" would be actively wrong.

### 12.9 Staged migration — Listen works after every stage

Ordered, each stage independently revertible, with the user live-testing between stages. **Stage 0 is not optional**: it is the safety net every later stage is checked against.

| Stage | What lands | Observable after it |
|---|---|---|
| **0. Freeze the contract** | A conformance test asserting **today's** Listen behaviour through the mock SDR: header keys, `ri16_le`/48000/960, status keys, a closed squelch as a `sample_index` jump plus `DISCONTINUITY`, refusal codes, retune-ends. Runs against the existing implementation. | No change at all. A test that fails loudly the moment any later stage drifts. |
| **1. Contracts** | This section plus ADR-0011 §8; `schema_version` 3 names reserved. | No change (documents only). |
| **2. Audio blocks and the `audio` output, inert** | `squelch`, `agc`, `deemphasis`, `audio_out`; the `audio` output kind; `input.liveness`; `recipes/analog-wfm.recipe.json`. | `GET /api/blocks` lists them, and a hand-started WFM audio pipeline is audible at `audio/<pipeline>/<output>`. **Both paths exist side by side**; `/ws/open/listen` is untouched. |
| **3. Parity harness** | The same fixture through the mock SDR into legacy Listen and into the recipe: audio compared sample-wise (or by level/SNR envelope), RDS PI/PS identical to the `hk_demod::rds` oracle, refinement converging to the same centre within T-070's tolerance, CPU within an agreed factor. | A green parity test. Still nothing user-visible. |
| **4. Opener switch, flag-gated (default off)** | `HK_LISTEN_PIPELINE=1` makes `/ws/open/listen` run chooser → ephemeral pipeline. Stage 0's conformance test runs in **both** modes. | Identical audio and UI with the flag on; instant revert by unsetting it. **This is the stage the user live-tests.** |
| **5. Default flip, WFM/NBFM/AM only** | The flag defaults on; SSB/CW keep `legacy` (§12.5); the legacy chain stays in the build. | Dock entries carry `pipeline_id`; audio pipelines appear in `GET /api/pipelines`; a `listen`-origin row exists once §11's table has landed. Reverting is one environment variable. |
| **6. One output model in the UI** | Dock entries become `{pipeline_id, output_id, kind}`; the FM panel reads RDS from the sibling `messages` output. | Several outputs per signal stack in one dock; RDS text comes from the pipeline that produces the audio. |
| **7. Retire the chain (never the opener)** | Delete `ChainKind::Listen`'s producer for the cut-over modes; map listener-budget accounting onto audio pipelines. **Only after stage 5 has been live-tested**, and only for those modes. | `/api/status` `listen.budget` still reports; `/ws/open/listen` unchanged. `chains/listen.rs` survives for SSB/CW permanently (U4 = A). |

**Stage 3 implemented (T-868, 2026-09-25).** `crates/hk-pipeline/tests/listen_recipe_parity.rs`: the real `fm_100p8M_2p4M` capture (channelised offline to 480 kS/s around the blindly-found station, so both chains see the same real samples) behind the mock SDR, legacy `open/listen` and `recipes/analog-wfm.recipe.json` side by side on one ring. Measured: the two audio streams, laid on the one capture-time axis, correlate **0.95–0.98** at < 1 ms lag; 100 ms level envelopes track within ~1 dB rms; the recipe's `station` output completes the `hk_demod::rds` oracle's PS (`Unstoppa`) under its PI (`1694`), both matching the fixture's hidden truth; the two refinements agree within T-070's 250 Hz (typically < 110 Hz), each within 2 kHz of truth. The harness swaps the recipe's `{node: crc}` objective (not yet run, T-870) for `{builtin: "wfm-pilot"}` to compare like with like. **CPU** (a `timing`-tier test, `just timing`): the recipe's audio branch costs **3.3–5.0×** the legacy chain on the same samples (debug build, loaded box), and RDS adds little on top — bounded at 6× as a regression guard, a cost to look at before stage 5's default flip. **Found and fixed:** legacy Listen stamped every audio record one probe window (~1 s) early — its time origin was the probe head, not the first demodulated sample — so its audio sat off the shared capture-time axis; the harness now asserts each stream against the oracle on the mock's own clock.

### 12.10 Where the framing breaks down (honest objections)

1. **Mode selection is not in the pipeline** (§12.2). "Listen is just a pipeline" is really "a chooser plus a pipeline". That is fine — but the chooser must be named and owned, or "no manual mode" quietly degrades into "pick a recipe by hand", which is a product regression dressed as a refactor.
2. **Latency is a contract for audio and a statistic for decoding** (ADR-0011 §8.5). Without `live-edge`, moving Listen onto the recipe runtime degrades live listening in a way no existing test would catch: everything still decodes, it just lags. **Highest-risk item in this plan.**
3. **The refinement objective is not a node metric** (ADR-0011 §8.7). Two objective forms is a wart; the alternative is a worse objective.
4. **Per-consumer versus named object** (§12.3). Two ownership modes are irreducible, and the attach-don't-duplicate rule is load-bearing rather than an optimisation.
5. **SSB/CW have no blocks** (§12.5) and writing them well is real DSP work this amendment does not fund. A permanent legacy path for them is the decided outcome (U4 = A).
6. **Stereo is a wire change *and* a block** (ADR-0011 §8.4): `ri16_le` mono is baked into every client. **In scope (U5 = yes, user 2026-09-23, overriding docs/20's "no"):** the user treats stereo as part of decoding the signal — the 19 kHz pilot and the 38 kHz L−R subcarrier are signal content the device should recover, not a listening nicety. See §12.13.
7. **What should *not* be done: retiring the opener.** `/ws/open/listen` should never be deprecated. The task title says "retire the special-cased Listen path", and the *chain* is worth retiring — the *verb* is not. Keeping a target-shaped one-shot entry point is what makes the product feel like a radio rather than a build system.
8. **And not yet: none of stages 2+ should start before M1's blocks and runtime are real.** `hk-blocks` and `hk-pipeline::recipes` are contracts with unmerged tasks; adding a second consumer of an unbuilt runtime is speculative. This is a contract now; code when the workbench runs.

### 12.11 Task-id collision (for the coordinator)

§11.6 and §11.9 originally proposed ids **T-230…T-238** for the candidate-pipeline work, including "Listen as an audio-output pipeline, per T-221's plan". **Those numbers are already assigned** in `docs/tasks.yaml` to unrelated M2/M3 tasks (T-230 classifier accuracy, T-232 temp-dir leak, T-234 M2 gate dwell, T-236 `hk-api` shutdown join, T-238 OTA abstention, …), all done or blocked. §11 was therefore converted to placeholder ids **CP-1…CP-9** (coordinator, 2026-09-15), matching the LP-* style below. Neither block is minted into `tasks.yaml` while MAUTO is unscheduled; the coordinator assigns real numbers at scheduling time.

This section therefore uses placeholder ids **LP-1…LP-8**, mapping onto §12.9's stages; the coordinator assigns real numbers.

| Id | Task | Stage | Deps | Model |
|---|---|---|---|---|
| LP-1 | Listen conformance freeze through the mock SDR | 0 | – | Opus, core_interface |
| LP-2 | Audio blocks + `audio` output kind + `input.liveness` + schema 3 | 2 | M1 runtime, LP-1 | Opus, core_interface |
| LP-3 | `recipes/analog-wfm.recipe.json` (audio + RDS siblings) | 2 | LP-2 | Sonnet (reviewed) |
| LP-4 | Parity harness: legacy chain versus recipe | 3 | LP-3 | Opus |
| LP-5 | Chooser (`hk_pipeline::audio::choose`), flagged opener switch, attach-don't-duplicate | 4 | LP-4 | Opus, core_interface |
| LP-6 | `refine.objective.builtin` + refinement applied as a hot edit | 4 | LP-2 | Opus, core_interface |
| LP-7 | MUI: one output model across the dock and the per-signal panels | 6 | LP-5 | Sonnet |
| LP-8 | Retire `ChainKind::Listen` for cut-over modes; listener-budget mapping | 7 | LP-5 live-tested | Opus |

### 12.12 Open questions (for the user)

*Consolidated with §10's and §11.10's lists in [docs/20, the MAUTO decision brief](../20-mauto-decision-brief.md) (T-553): Q1, Q3 and Q5 are decided there (Q5 duplicates §11.10 Q2 and is already answered by §12.8); Q2 is merged with §10 Q5 and Q4 stands, both for the user.*

**Answered 2026-09-23:** Q1 → docs/20 D5 (retune still ends the stream); Q2/Q3 → U4 = A + D6 (SSB/CW legacy permanently, chain kept for them); **Q4 → U5 = yes, stereo in scope (§12.13)**; Q5 → D3.

1. **Retune behaviour.** Keep "a class-changing retune ends the audio stream and the client reconnects" (proposed — it is today's tested behaviour), or make audio pipelines survive a retune (nicer, but a live-visible change with no coverage)?
2. **SSB/CW.** Fund `ssb_demod`/`cw_demod` blocks so every mode is a recipe, or accept a permanent legacy path for them (proposed)?
3. **Keep the legacy chain indefinitely** as a simple, dependency-free live-audio fallback for when a recipe misbehaves, or delete it once WFM/NBFM/AM are cut over (stage 7)?
4. **Stereo audio.** Wanted at all? It is a wire change (`channels`, interleaved `ri16_le`) plus a `stereo_decode` block, and nothing asks for it today. — **Answered: yes** (U5, user 2026-09-23; §12.13).
5. **Promotion.** Confirm §12.8's reading: a pipeline claims a *set* of output kinds and promotion conflicts per kind (this supersedes §11.10 open question 2's phrasing).

*Unverified in this amendment: that a recipe chain reaches the current Listen path's audio quality and CPU cost (stage 3 measures it); the 0.6 s `max_backlog_s`; that `squelch`-as-a-block's zero-item chunks are indistinguishable at the UI from today's `sample_index` gaps; and whether the wrapped chooser's mode decisions match today's exactly (stage 0's test pins the behaviour it must reproduce).*

### 12.13 Stereo audio is in scope (U5, user 2026-09-23)

The user's answer to §12.12 Q4 is **yes**, contrary to docs/20's recommendation. Stereo is part of decoding a broadcast-FM signal, so it belongs to the pipeline, not to a later nicety. What that commits, stated so the implementing tickets can be sized (placeholder ids; the coordinator allocates real ones):

| Id | Task | Deps | Model |
|---|---|---|---|
| LP-9 | `stereo_decode` block: 19 kHz pilot PLL, 38 kHz L−R demodulation, matrix to L/R, a pilot-lock `Status` (and S1 `evidence()` via the pilot lock the refinement loop already reads); mono fallback when the pilot is absent or unlocked, never a silent mono labelled stereo | LP-2 | Opus, core_interface |
| LP-10 | Wire change: `audio` stream header gains `channels` (1 or 2) with interleaved `ri16_le` frames, a stream-contract version bump, and every client updated in the same change (UI dock/`AudioSession`, `py/examples/hk_audio_wav.py`, the documented TCP one-liners) plus `docs/api.md` and `api_contract` tests. A mono stream stays byte-identical to today's. | LP-9, LP-1 (the conformance freeze must pin mono first) | Opus, core_interface |

- **The mono contract does not break silently.** A client that ignores `channels` must still get mono unless it asks for stereo (opt-in on the opener or the output), so LP-1's frozen behaviour holds.
- **Honesty.** A stream is labelled stereo only while the pilot is locked; losing lock mid-stream is reported (a status change), not hidden. This is the same rule as never implying resolution the front end did not deliver.
- ADR-0011 §8.4's "Stereo is also out" is superseded by this section.
- **LP-10 implemented (T-874, 2026-09-24).** Stream contract **1.5** (§12.2): `listen?…&channels=2` is the opt-in (absent or `1` = mono, anything else `400`); `audio.channels` is what the demodulator delivers — 2 only on a WFM channel, whose records are 960 interleaved `L, R` frames, with `sample_index` still counting frames; two-channel status records carry `stereo` (pilot locked, L−R decoded) and `stereo_lock_losses`. The legacy chain decodes L−R in `hk_demod::WfmDemod` (pilot-PLL `2θ`, a copy of the mono path's own filter and de-emphasis, `L = R = M` exactly while unlocked); a mono stream is unchanged apart from the header `version` string (1.2 → 1.5, which re-syncs it with the document). Clients updated together: the UI dock asks for stereo and plays two channels (`AudioSession`, `jitter.ts`, the worklet) and says "stereo" only from the status; `py/examples/hk_audio_wav.py --stereo`; the documented TCP one-liner (docs/api.md). Frozen in `crates/hk-pipeline/tests/listen_conformance.rs` §9. **Not yet:** a recipe `audio` output stays mono (`audio_out` takes one input) — the recipe side of stereo (`stereo_decode` → a two-input `audio_out`) is the next step, and the `playback` opener is mono.

---

## 13. Amendment — dependence, calibration reach and ADC fill (T-616 / T-617 / T-618, 2026-09-21)

**Status:** PROVISIONAL, planning only, no code. Source: **T-547**'s measurement,
[docs/21 §5.1, §5.2, §5.3, §3, §4](../21-evidence-bits-under-quantisation.md). §§1–12 stand
except where a delta is listed in §13.6.

**Composes with, and does not touch, [ADR-0022](0022-false-confirm-budget.md).** That ADR already
removed every calibrated metric from the confirm gate (§2.1: only analytic-null bits pay, and
`analytic_holdout_bits`, not `evidence_bits`, is the confirm key). Nothing below moves a
confirm threshold, re-derives the 24 bits, or admits a calibrated bit toward them — ADR-0022 §9
reserves that admission for an explicit amendment to *that* ADR, and this is not it. What
follows fixes `evidence_bits` **where ADR-0022 left it**: as the search-order key, the
result-rank key, the `floor_j` pruning key, and the number the UI shows a user as a
significance. ADR-0022 §9's own table anticipated exactly this split — "the damage lands on
§1.3's `floor_j` values and 12-bit caps, on search order, and on whether `evidence_bits` may be
shown to a user as a significance at all — all outside this gate."

**The three defects, as measured.** (1) §1.3 sums `b_j` over stages and, inside a stage, over
metrics; a sum of significances is a log-probability only under independence, and the metrics
are not independent (docs/21 §5.1: `snr`↔`evm` ρ = −0.989, `line_violations`↔`bit_structure`
−0.706, `timing_var`↔`snr` −0.52). (2) A quantile table cannot express every level asked of it
(docs/21 §5.2, §5.3). (3) The conditioning variable §2.2 named — "gain states" — is the wrong
one (docs/21 §3, §4).

---

### 13.1 Dependence groups: within a group, the **maximum**, not the sum (T-616)

#### The rule

`Evidence` gains a required `group: GroupId` alongside `metric`. For a stage *j*:

```
b_j = Σ over declared dependence GROUPS g at stage j of   max over metrics m in g of  b_{j,m}
```

then capped by `cap_j` (§13.3). The sum survives only **between** groups. It never runs over
two metrics a block has declared, or failed to declare, as dependent.

**Direction convention.** Groups are defined on the metrics' **bits**, not their raw values.
`snr` is evidence when large and `evm` when small, so their raw Spearman ρ = −0.989 is
ρ = **+0.989** in the evidence direction. A group is a set of metrics whose *bits* are
non-negatively dependent under the null.

#### Why maximum, and what it costs

For two perfectly dependent tests, `P(both exceed) = P(max exceeds)`, so the group's true
significance *is* the maximum: the rule is exact at the limit the measurement found
(ρ = 0.989 is that limit). For independent tests the true value is the sum, and the maximum
**under-reports** — by up to `(k−1)·6` bits for a group of *k* metrics at the §13.3 per-metric
ceiling. That direction is the point. **An `evidence_bits` the system cannot justify must read
as less evidence, never as more**, which is the same rule that makes `BiasTee::Unknown` not
`Off` and an unclassifiable diff run the full gate.

The cost is **recall in the search, never validity of a confirm**. Three places feel it:

1. **`floor_j` pruning.** A stage can no longer clear its floor by presenting two near-copies of
   one statistic. On the M1 FSK ladder, S2/S3 lose roughly 6–12 bits of headroom per stage. Some
   true-but-weak signals will prune where they previously survived; §1.3's "pruning is never
   total" keeps the best pruned node as a partial result, so they degrade to partials, not to
   silence.
2. **Beam order.** Candidates re-order. Nothing about the ranked output's *meaning* changes.
3. **What a user is shown.** The displayed `quality = 1 − e^(−bits/8)` falls for correlated
   ladders. It was previously overstated.

Nothing reaches the confirm gate, because ADR-0022 §2.1 already put a wall there.

#### The alternatives, and why not

| Residual | Why rejected |
|---|---|
| **Drop one of each correlated pair** (publish `snr`, not `evm`) | Discards the metric permanently in the contract, and the choice is arbitrary: the pair is one statistic *in this block version*, not in the abstract. A later block computing EVM decision-directed after the slicer would have a genuinely separate statistic and no way to publish it. The maximum keeps both published and lets the group split on evidence (§13.5). |
| **Decorrelate — whiten, or score the metric vector jointly** | Correct in principle and unaffordable in practice. A joint tail needs a joint calibration table, whose sample requirement grows with dimension, against a ceiling of `log₂ N` bits per cell (§13.2) that is *already* the binding constraint at one dimension. It also makes the table's validity depend on the correlation structure holding, which is the thing that moves. |
| **Keep the sum, charge a measured dependence penalty** | Fails **open**. The penalty is a function of a correlation measured on one corpus; when the correlation shifts on real hardware the penalty is stale, and a stale penalty that is too small reports *more* evidence than is there. A rule whose error mode is over-claiming is not admissible on a number the search prunes and ranks with. |

#### Declaration is mandatory, and the default is one group

A block publishing more than one metric at a stage **must** declare the partition, and must
ship, in its calibration file (§13.2), the measured rank-correlation matrix over the metrics it
publishes, on the same corpus and at the same `n` as the tables.

> **Undeclared is not independent.** If a block publishes two metrics at a stage without a
> declared partition, or its calibration file carries no correlation matrix, the engine treats
> **every metric that block publishes at that stage as one group** and takes the maximum. A
> block that wants to be paid for two metrics must show they are two.

A pair may be declared as separate groups only if the shipped matrix shows |ρ| below a stated
threshold — **0.3** — in every fill bucket the file covers, at the file's own `n`. Anything
between 0.3 and the perfect-dependence limit is treated as one group; there is no partial-credit
tier, because the measurement that would size one (docs/21 §2, "not resolvable at n = 820")
does not exist.

#### The groups that exist today

For the M1 blocks docs/21 measured, at n = 112 symbols:

| Stage | Block | Group | Members | Measured ρ (evidence direction) |
|---|---|---|---|---|
| S1 | `fsk_demod` / `am_demod` | `demod_shape` | `bimodality` | — (singleton; ρ ≤ 0.13 against all others) |
| S1 | any | `pilot` | `pilot_lock`, `offset_ratio` | **unmeasured** → one group by the default rule |
| S2–S3 | `fsk_demod` | `soft_quality` | `snr`, `evm`, `timing_var` | `snr`↔`evm` **0.989**; `timing_var`↔`snr` 0.52 |
| S2 | `clock_recovery` | `eye` | `eye_open` | — (singleton; ρ ≤ 0.054 against all others) |
| S3 | slicer / line-code | `bit_shape` | `line_violations`, `bit_structure` | **0.706** |

`timing_var` joins `soft_quality` on 0.52, which is above the 0.3 threshold: it is a Gardner
loop error shrinking with SNR, not an independent look at the signal. `eye_open` stays a
singleton on the measurement — and is separately crippled by §13.2, which is a different
problem with the same consequence.

S4–S6 metrics (`sync_excess`, `sync_regularity`, `check_distinct_valid`, `field_fit`,
`identity_recurrence`, `plausibility`) are **not covered here**: their nulls are analytic
(§2.2's first list), they are the ones ADR-0022 §2.1 lets pay for a confirm, and their
dependence is a separate question that ADR-0022 §3.2's margin already prices. This amendment
changes nothing about them.

**Blocks not yet measured.** `am_demod` envelope bimodality, any future Costas/PSK path and the
C4FM path are unmeasured (docs/21 §7), so every metric each publishes at a stage is one group
until **T-619** measures otherwise. That is the default rule doing its job, not a gap.

---

### 13.2 A calibration table declares what it can express (T-617)

#### The problem, restated from measurement

`eye_open` under the noise null takes **66 distinct values** over 4200 windows at n = 112, with
an atom sitting exactly at the 4-bit and 6-bit quantiles: asking that table for a 6-bit
threshold returns one worth **2.41 bits** (docs/21 §5.2). The float reference is wrong by
3.6 bits *before any ADC is involved*, so this is a property of the statistic at that support.
Separately, a table sampled with N windows bounds any significance at **log₂ N** — 12.04 bits at
N = 4200 (docs/21 §5.3) — against §1.3's caps of 12 for S0–S3 and 32 for S4.

Today the table answers both questions silently and wrongly. The fix is that it must **declare
its reach and refuse beyond it**.

#### Schema — `synth/calibration/<block>.json`, `hackriff.calibration/1`

§2.2's one-line description ("quantiles of `raw` under noise and a mismatched-parameter null, at
stated `n`") is replaced by this document. It is normative; `raw`-only tables are not loadable.

```jsonc
{ "schema": "hackriff.calibration/1",
  "block": "fsk_demod@3",              // name@version; a version bump invalidates the file (§2.2)
  "generated_utc": "2026-09-21T00:00:00Z",
  "generator": "py/hkpy/calibrate.py@<git-sha>",   // reproducibility, not decoration

  "conditioning": { "key": "adc_fill_sigma_lsb",   // §13.3; the ONLY permitted key in v1
                    "bucket": "nominal",           // nominal | (see §13.3 for the degenerate end)
                    "sigma_lsb_range": [0.5, 77.0],
                    "clip_fraction_max": 0.30 },

  "correlation": {                     // §13.1: mandatory when > 1 metric is published at a stage
    "method": "spearman", "windows": 820, "corpus": "matched@20dB",
    "metrics": ["snr", "evm", "timing_var", "eye_open"],
    "rho": [[1.0, 0.989, 0.519, 0.013], [0.989, 1.0, 0.524, 0.028],
            [0.519, 0.524, 1.0, 0.009], [0.013, 0.028, 0.009, 1.0]] },   // evidence direction
  "groups": { "S2": { "soft_quality": ["snr", "evm", "timing_var"], "eye": ["eye_open"] } },

  "tables": [
    { "metric": "eye_open",
      "null": "noise",                 // noise | mismatch:symbol_rate | mismatch:centre | mismatch:deviation
      "n": 112,                        // SUPPORT (symbols/bursts/frames) — a table is per-(metric, null, n, bucket)
      "windows": 4200,                 // N actually sampled
      "sample_ceiling_bits": 12.04,    // = log2(windows). A hard bound, not a target.
      "distinct_values": 66,           // the §5.2 diagnostic, carried so the defect is visible in the file

      "levels": [                      // ONLY levels the table can express. Ordered, no gaps implied.
        { "bits": 1.0, "threshold": 0.0412, "realised_bits": 1.00 },
        { "bits": 2.0, "threshold": 0.0630, "realised_bits": 1.99 },
        { "bits": 3.0, "threshold": 0.0881, "realised_bits": 2.97 }
      ],
      "unexpressible": [               // levels asked for during generation that the support cannot carry
        { "bits": 4.0, "realised_bits": 2.41, "reason": "atom" },
        { "bits": 6.0, "realised_bits": 2.41, "reason": "atom" }
      ],
      "admissible_bits": 3.0           // = min(max expressible level, calibrated_claim_cap, sample_ceiling_bits)
    }
  ] }
```

- A **level is expressible** when the realised significance of its threshold is within
  **0.25 bits** of the claim. That tolerance is the generator's, and it is stated in the file
  rather than assumed by the reader.
- `levels` is a **list of the admissible answers, not a curve**. There is no interpolation
  between levels and no extrapolation beyond the last one, in either direction.
- The file is per **(metric, null, n, fill bucket)** cell. Supports are enumerated, never
  interpolated: docs/21 §5.2 and §5.3 are both support-dependent, so "the table at some other
  `n`" is not a table.

#### What a caller gets — the refusal

Two directions of use, both declared:

```rust
/// raw -> bits (scoring). Saturates at `admissible_bits`; never interpolates above the top level.
pub enum Score {
    Bits { bits: f32, level: f32 },
    Saturated { admissible_bits: f32 },       // credited, but flagged as a floor on what is knowable
    NoTable { cell: CellId },                 // credits 0.0 bits
}

/// bits -> raw (a floor or threshold test, e.g. `floor_j`). May REFUSE.
pub enum Threshold {
    At { bits: f32, raw: f32 },               // an expressible level, snapped UP (never down)
    Shortfall { requested_bits: f32, admissible_bits: f32, reason: Unexpressible },
    NoTable { cell: CellId },
}
pub enum Unexpressible {
    Atom { realised_bits: f32 },              // eye_open @ n=112: requested 6.0, realised 2.41
    AboveSampleCeiling { ceiling_bits: f32, windows: u32 },
    AboveCalibratedClaimCap { cap_bits: f32 },
}
```

> **A calibration table never answers a question it cannot answer.** Asked for a level it does
> not hold, it returns `Shortfall { requested_bits, admissible_bits, reason }` and the caller
> gets the *achievable* number or nothing — never the raw value sitting at that quantile. Asked
> for a cell it does not have, it returns `NoTable` and the metric contributes **0.0 bits**.
> Silently returning the nearest quantile, interpolating between levels, or extrapolating past
> the last one are all forbidden: they are the behaviour that made `eye_open` report 6 bits of
> evidence for 2.41 bits of fact.

Concretely, the defect docs/21 §5.2 found now surfaces as:

```
Shortfall { requested_bits: 6.0, admissible_bits: 3.0,
            reason: Atom { realised_bits: 2.41 } }
```

#### Consequence for `floor_j`

§1.3's floors (6 bits S0–S3, 10 bits S4–S5) are tests in the **bits → raw** direction, so they
are now subject to the refusal:

- A floor is **snapped up** to the nearest expressible level ≥ `floor_j`. Never down.
- If no expressible level ≥ `floor_j` exists at or below `admissible_bits`, the floor is
  **unreachable for that cell**. The engine does not lower it and does not pretend it was met:
  it marks the cell `floor_unreachable`, surfaces it as a **calibration defect** on the job, and
  the node survives only as a §1.3 partial result. `eye_open` at n = 112 is exactly this case
  against a floor of 6.
- The remedy is more support (a larger `n` gives more distinct values) or a floor restated with
  its reason, and both are generation-time decisions with a name on them — not a runtime fudge.

#### Consequence for the caps

`cap_j` was sized for a sum of independent metrics. Three ceilings now bind **before** it:

1. **Per-metric calibrated claim cap = 6.0 bits.** docs/21 §2 measures δ = 1.8 bits at a claimed
   6 and 3.3 at a claimed 8, and cannot resolve whether the law is additive or saturating at
   n = 820. Above 6 the discount is *unmeasured*, not small. No calibrated metric may claim more
   than 6 bits until a ~30 000-window mismatched-null corpus says otherwise.
2. **`sample_ceiling_bits = log₂ N`** per cell, in the file.
3. **`admissible_bits`** per table, the minimum of the three.

`cap_j = 12` for S0–S3 therefore stops binding in practice — a stage with two declared groups
tops out at 12 anyway under (1) — and is retained only as a belt. **`cap_j = 32` for S4 stands,
and is reachable only analytically**: `sync_excess` has a closed-form null, so its bits are not
bounded by any table's N. No calibrated metric can approach it, and §2.2 should never have
implied one could. A calibrated metric claiming above `admissible_bits` is a loader error, not a
capped value.

---

### 13.3 Condition on **ADC fill**, not gain — and budget the sampling (T-618)

#### The conditioning key

§2.2's `conditioning.key` is **`adc_fill_sigma_lsb`**: the per-component noise σ of the window
expressed in ADC LSB. It is **not** the LNA/VGA/amp setting and **not** the clip fraction.

The control that makes this attributable is docs/21 §4: applying 51 dB of gain with the ADC
skipped reproduces the float table to **0.02 bits on every metric**, and leaves the
demodulator's success rate bit-identical at 0.1876 in every column. Every metric in the set is a
ratio or a power-normalised statistic, so gain on float IQ is exactly a no-op. §2.2's phrase
"across gain states" named a variable that cannot move these numbers.

Clipping, the expected suspect, is **refuted**: 28.4 % of samples clipped costs ≤ 0.34 bits at a
6-bit claim (docs/21 §3). Under-fill is the hazard: at σ = 0.21 LSB the worst metric over-claims
1.64 bits, `eye_open` collapses to 0.91 bits realised for a 6-bit claim, and the demodulator's
success rate jumps from 19 % to 53 % — the mechanism is not added quantisation noise, it is that
**the metric is measuring a different thing**.

#### The buckets — two, because two is what the measurement buys

| Bucket | Definition | Table? |
|---|---|---|
| `nominal` | σ ≥ 0.5 LSB **and** clip fraction ≤ 0.30 | **Yes — one table per (metric, null, n).** δ ≤ 0.34 bits across the whole range, σ = 0.57 → 77 LSB. |
| `under_filled` | σ < 0.5 LSB | **No table.** Calibrated metrics contribute **0.0 bits** and the window is marked. |
| `over_clipped` | clip fraction > 0.30 | **No table.** Unmeasured on the null side; same treatment. |

The boundary at **σ = 0.5 LSB is derived, not chosen**: it is the only boundary docs/21 §3's
sweep supports. The rows from σ = 0.57 to σ = 77 LSB sit within 0.34 bits of each other, so an
interior boundary would be manufacturing a distinction the data cannot see; the row at σ = 0.21
is 1.64 bits away and breaks a metric outright. Two regimes are what was measured, so two
buckets are what get built. Similarly, `over_clipped` is a boundary of the **measurement**, not
of the physics — the null corpus reached 28.4 % clipped and stopped — and it gets no table for
that reason. (The *matched* corpus survived 76 % clipped almost unchanged, which is encouraging
about recall and says nothing about a null tail.)

This costs **one cell per (metric, null, n)**, not five. That matters against §13.2's sample
budget below, and it is the practical argument for deriving bucket boundaries rather than
picking a comfortable-looking ladder of them.

#### The degenerate end — the cheap runtime rule

> Over the evaluation window, compute the per-component sample standard deviation in LSB
> (`σ_LSB = std(re) ⊕ std(im)` on the pre-scaling integer samples). If **σ_LSB < 0.5**, every
> calibrated metric in that window scores `NoTable` — 0.0 bits — the window is flagged
> `under_filled`, and the analytic metrics (S4–S6) are unaffected.

It is one pass over samples already in cache, it runs once per evaluation window and not per
metric, and it fails closed: an under-filled window produces *less* evidence, never more. It
does **not** stop the demodulator, stop capture, or change a gain — under-fill is a property of
the recording, and a recording cannot be re-taken from the analysis path.

Note how far the boundary is from practice: four of docs/21 §6's five real captures sit at
σ = 2.0–2.4 LSB and one at 43.6 LSB. None is under-filled. The rule is a guard, not a common
path.

#### Provenance — which field the runtime reads (docs/07 §2.x)

- **`Provenance.quantisation_limited`** already exists, defined as "noise floor within 3 dB of
  the ADC quantisation floor" (docs/07, spike S4). It is the right *shape* — an under-fill
  indicator — but it is a boolean at a 3 dB threshold, and the bucket boundary is a number.
- **`Provenance` gains `noise_sigma_lsb: f32`** (per-component noise σ in LSB, at the tune the
  record pins), so the bucket is computed from a recorded quantity rather than re-derived, and
  so a stored `Evidence` row's bucket is reconstructible after the fact. `quantisation_limited`
  stays as the coarse flag and the two must agree; a disagreement is a front-end bug worth
  surfacing.
- The per-capture **clip count** already rides on `Detection.clip_count` / the sticky
  `Provenance.overload` flag (docs/07); the `over_clipped` bucket reads those. No new clip field.
- **Missing `noise_sigma_lsb` is `under_filled`, not `nominal`.** An unknown fill is not a good
  fill, for the same reason `BiasTee::Unknown` is not `Off`.

#### The sampling budget, stated rather than discovered

Per **(block, metric, null, support n, fill bucket)** cell:

| Quantity | Value | From |
|---|---|---|
| Hard bound on any claim | `log₂ N` bits | docs/21 §5.3 |
| N to *touch* 12 bits once | **4 096** | same |
| N for a usable interval at 12 bits | **~40 000** | same |
| N for ~64 exceedances at the 6-bit claim cap | **4 096** | `64 · 2⁶` |
| **Adopted floor** | **N = 4 096 windows per cell** | the two agree; the cap at 6 bits (§13.2) is what makes 4 096 sufficient rather than 40 000 |

Cells for one block, as built: **1 fill bucket** (§13.3) × **3 nulls** (noise, mismatched rate,
mismatched centre) × **≥ 2 supports** `n` = **6 cells**, so **≈ 25 000 windows per block
version**. That is the number to budget in M-2 and the number a block-version bump costs. Had
the buckets been a five-step gain ladder — the retrofit §2.2's wording invited — the same block
would have cost ~125 000 windows for distinctions the measurement cannot see.

The file's `windows` field is what a reader believes; a cell generated with fewer than the floor
is loadable but must declare its lower `sample_ceiling_bits`, and the refusal of §13.2 then does
the rest. **Under-sampling is visible in the answers, not only in the generator's log.**

---

### 13.4 What is *not* changed here

- **ADR-0022's gate.** `min_analytic_holdout_bits = 24`, `hard_check_floor_bits = 16`,
  `min_check_width = 8` and §4.2's frame formula all stand untouched. None reads a calibrated
  metric, by construction (ADR-0022 §2.1), so none of the three defects above ever reached them.
  ADR-0022 §9's admission of conditioned calibrated bits toward the 24 stays **unexercised** —
  and §13.1 raises its price, because the honest per-stage calibrated contribution under the
  maximum rule is smaller than the sum that admission was imagined against.
- **The look-elsewhere accounting.** `L_j` per stage, and ADR-0022 §2.3's rule about which term
  a confirm reads, are unchanged. §13.1 changes how `b_j` is assembled from metrics, not how the
  multiplicity is charged.
- **Analytic nulls** (§2.2's first list). Untouched throughout.
- **The one-way door.** §11.5 and `change_emitter_lifecycle` still mean no rule demotes a
  confirmed emitter. Every rule above moves `evidence_bits` **down** or refuses to answer, so
  none of them can newly open that door.

### 13.5 What would falsify these choices

1. **§13.1's maximum.** A block that publishes `snr` and `evm` from genuinely different
   estimates — EVM decision-directed after the slicer, SNR from the channel power ratio — and
   ships a correlation matrix showing |ρ| < 0.3 in every fill bucket at the file's `n`, on
   ≥ 4 096 windows. Then the group splits and the sum returns *for that block version*. This is
   the designed exit, not a loophole: the declaration is per block version and evidence-backed.
2. **§13.1's cost.** If the blind acceptance corpus (docs/22) shows the maximum rule materially
   lowering the a-priori solve rate — true signals pruned at S2/S3 that the sum used to carry —
   then the cost is not acceptable and the answer is a lower `floor_j`, with the floor's new
   value stated against that measurement, **not** a return to the sum. Filed as **T-660**.
3. **§13.2's 6-bit claim cap.** A ~30 000-window mismatched-null corpus that resolves whether
   δ is additive or saturating in the claim (docs/21 §2 says n = 820 cannot). If it saturates,
   the cap rises and the 12-bit `cap_j` starts binding again.
4. **§13.3's single `nominal` bucket.** Any metric showing > 0.5 bits of δ spread *within*
   σ ∈ [0.5, 77] LSB on a larger corpus, or any null-side measurement beyond 28 % clipped,
   splits the bucket. The budget above then multiplies by the number of buckets, which is why it
   is stated per cell.
5. **All of it, on blocks that were never measured.** Everything above rests on the FSK path at
   n = 112 plus one classifier feature. **T-619** extended the measurement to AM/OOK and C4FM
   (docs/21 §10, 2026-09-22): §13.1's rule survives and its table gains two groups
   (`{eye_open, snr, evm}` on AM/OOK, `{evm, offset_ratio}` on C4FM, the first at ρ = 1.000
   exactly), and **item 4's trigger fired** — the null side was measured to 77 % clipped and the
   AM/OOK metrics over-claim 5–6 bits there, so the `nominal` bucket must split or tighten
   (measured: σ ≥ 1.0 LSB and clip ≤ 10 % holds every path to ≤ 1.2 bits at a 6-bit claim). That
   amendment is not taken here. docs/21 §10.4 also shows the runtime rule must measure the
   **noise floor's** fill, not the window's.

### 13.6 Deltas to §§1–12

- **§1.3** — `evidence_bits = Σ_{j≤k} min(b_j, cap_j) − L_j` stands, with `b_j` now defined by
  §13.1 (sum over groups of the maximum within a group) rather than a bare sum over metrics.
  `floor_j` is subject to §13.2's snap-up and `floor_unreachable`. The caps are restated by
  §13.2: 12 for S0–S3 is a belt behind the 6-bit per-metric calibrated cap; 32 for S4 is
  analytic-only. The §1.3 note ADR-0022 §11.1 added — `evidence_bits` is the search and rank
  key, not the confirm key — is unchanged and is what makes all of this non-load-bearing for a
  confirm.
- **§2.1** — `Evidence` gains `group: GroupId`. `MetricId` is unchanged (no metric is added or
  removed). The ≤ 4-entries-per-call and no-allocation rules are unchanged.
- **§2.2** — the calibrated-null bullet is replaced by §13.2's schema and §13.3's conditioning.
  The sentence "whether these tails are stable enough across 8-bit quantisation and **gain
  states**" is superseded: the variable is ADC fill. The "checked by a Rust test that re-samples
  1 000 noise windows" line stands and gains a second obligation — the test must also assert
  that a table **refuses** a level above its own `admissible_bits` (T-660).
- **§3.1 / §3.4** — no contract change; the beam simply orders on the new `b_j`.
- **§13 does not touch** §4 (templates), §5 (analyze API), §6 (bursts), §7 (evaluation),
  §11 (`CandidatePipeline`) or §12 (Listen).

*Unverified in this amendment: that the 0.3 grouping threshold is the right cut (nothing
measures the 0.3–0.9 band); that 0.25 bits is the right expressibility tolerance; that
`floor_j = 6` remains achievable at S2/S3 under the maximum rule on a real corpus (T-660); and
every number inherited from docs/21, which measured one block family at one support.*

---

## 14. Amendment — the incremental region-decode contract (T-265 = [ADR-0017](0017-time-extent-signal-model.md) TM-10, 2026-09-22)

**CONTRACT ONLY.** `POST /api/analyze` still answers `501` for a selection or band target and MAUTO is unscheduled, so nothing here runs a decoder, reads the ring or spawns a thread. ADR-0017 §9 blocked TM-10 on exactly that — *"attempting this earlier means building an incremental scheduler for a pipeline that does not exist"* — while leaving §6 as "a contract now; code when MAUTO is scheduled". This section is that contract written down where the analyze engine will read it, plus the types that make its four failure modes unrepresentable: `hk_pipeline::region` (T-265).

### 14.1 What it is a contract for

CLAUDE.md invariant 5: *"decode operates on a captured region and extends with it; live decoding **extends the region's time extent** and decodes only the newly-arrived part (incremental), never re-decoding what is already done."*

ADR-0017 §6.2 turned that into two rules, and the load-bearing clause is that **the policy belongs to the reader, not to the pipeline, the recipe or the signal**:

> **Rule L** — a reader attached at the live edge obeys ADR-0011 §8.5 unchanged: never a backlog, skip forward when behind, count and flag every skip.
>
> **Rule I** — a reader that is a bounded region of the ring processes `[t_start, t_end]` exactly once; extending the region enqueues only `[old_end, new_end]`.
>
> **A pipeline has exactly one input reader, so it is exactly one of the two.**

§5's `AnalyzeJob` is a Rule I pipeline. §12's Listen is a Rule L pipeline. §6's burst path is Rule I with a discontinuous acquisition. Nothing in this ADR was ever both, and this section is what stops the first implementation making one.

### 14.2 The objects

| Object | What it is | Rule it carries |
|---|---|---|
| `ReaderPolicy` | `live-edge {max_backlog_ns}` \| `bounded-region` | one field, so a pipeline cannot declare both |
| `PipelinePlan` | one reader policy + the outputs hanging off it | §6.2's "exactly one of the two", structurally |
| `ReadLedger` | the spans a reader **actually read**, its skips, and the evidence credited from them | §6.3's coverage honesty and §6.4's evidence rule |
| `RegionJob` | a Rule I job: extent, work queue, ledger, phase | §6.2 Rule I |
| `RegionPhase` / `Handover` | `Region` → `HandedOver {at, live_from}`, and the record of what fell between | §6.3's "explicit, named state … never an implicit merge" |

**Extent is not coverage.** A `RegionJob`'s *extent* is what the user asked for and it only ever grows; its *coverage* is what the reader got, and it is less whenever the ring had evicted part of the window, a segment boundary fell inside it (§5.3), or a handover abandoned enqueued work. The difference is recorded as skipped time and flagged, never rounded up — *"a coverage bar that silently has holes in it is worse than no coverage bar"*. The acceptance sentence is therefore **"a region job's coverage is exactly the samples it read"**, and it is a test, not a prose claim.

**Closed intervals.** Spans are docs/07 §4 `TimeRange`s, closed `[start, end]`. Two are *contiguous* when the later starts exactly at the earlier's `end` — they meet at one instant of zero duration, so summed durations still equal the union's — *overlap* only when it starts strictly before (the re-read, refused), and leave a *gap* when it starts strictly after. Extending by `[old_end, new_end]` is the ADR-0017 §6.2 formula verbatim, and it is exactly-once under this reading.

### 14.3 The four failure modes, and where each is refused

ADR-0017 §6.3 names two ways to fuse the readers and says of both that *"one of the two contracts breaks silently — always the worst kind"*. Each now has a named error rather than a paragraph:

| § | The mistake | Refused by |
|---|---|---|
| §6.2 | re-decoding what is done | `RegionJob::extend_to` answers `None` for an end already handed out; `ReadLedger::read` ⇒ `AlreadyRead` for a span overlapping one already read |
| §6.3 | fuse onto the **region** reader — audio acquires the batch job's backlog (§12.10's "highest-risk item") | `PipelinePlan::validate` ⇒ `AudioOnBoundedRegion` |
| §6.3 | fuse onto the **live-edge** reader — the skip discards the samples the job promised to process exactly once, and the job reports complete coverage over a region with holes | `ReadLedger::skip_to` ⇒ `SkipForbidden` under `bounded-region` |
| §6.4 | a sibling decode output's evidence `n` inflated by time the audio reader skipped | `PipelinePlan::policy_for_output` has no per-output override; `ReadLedger::credit` ⇒ `NotRead` for a span that was not read |

The last one is the one an implementer is most likely to get wrong while believing they are being generous. ADR-0011 §8.9's FM recipe has an `audio` output *and* an RDS `messages` output off the same `fm` node: **one reader, two outputs**, so RDS inherits the audio reader's live-edge policy and its text has a gap whenever the audio skips. That gap is correct and deliberate — the alternative buffers for RDS's benefit and makes the audio late — and it is recorded as a `DISCONTINUITY`. What must not happen is the §2.2 bits ladder counting the skipped seconds as evidence, which is why evidence is credited **against a span in the ledger** rather than against wall time. A user who wants gapless RDS runs a region job over the ring: Rule I, a second pipeline, and exactly the §6.3 shape.

### 14.4 Handover

A region job whose coverage catches the live edge **may** hand over to the live pipeline. That is one transition, out of one state, and it produces a record:

`Handover {at, live_from, gap: Option<TimeRange>, abandoned_ns, flags}` — `gap` is `Some` when the live reader starts after the job's coverage ended; `abandoned_ns` is work that was enqueued and never read; either puts `DISCONTINUITY::GAP` on the record, and only a handover that loses neither is `is_seamless()`. After it the job owns no samples: `extend_to`, `complete` and a second `hand_over` all answer `HandedOver`. Continuing to decode means opening a new job, which is a new reader — never the old one re-pointed at the live edge.

### 14.5 What this does **not** decide

- **No engine.** No scheduler, no thread, no ring read, no `/api/analyze` behaviour change, no route and no schema. `AnalyzeJob.window` (§5.2) already carries `segments`/`samples`/`gaps`; when the engine lands it fills them **from the ledger** rather than from the requested window, and that is the only §5 delta this foresees.
- **No persistence.** Nothing here is stored. A job's coverage lives as long as the job; `emitter_synthesis` (§5.4) keeps the verdict, not the coverage bar.
- **Scrub-back audio is still unruled.** ADR-0017 §6.5 flagged it and this does not settle it: `AudioOnBoundedRegion` refuses audio on a *bounded-region* reader, which is the fusion, and says nothing about a future playback reader with its own policy. If one is added it is a **third** `ReaderPolicy`, declared as such, not a bounded region with the audio rule quietly relaxed.
- **No number is introduced.** `max_backlog_ns` is carried, not defaulted: the runtime's value is `ListenConfig::max_backlog_s` (or a recipe's `input.liveness.max_backlog_s`), and a second spelling of it would be a new drift surface.

*Unverified in this amendment: that the work-queue shape survives contact with the beam search's re-entrancy (§3.1 may want to re-run a stage over a span already read, which is a **re-analysis** of read samples rather than a re-read and is legal under Rule I, but nothing measures it); and whether the UI wants a handover offered automatically when coverage reaches the live edge, or only on request — §6.3 says "may hand over" and this contract does not choose.*

---

## 15. Amendment — protocol facts cross as template data: fact provenance, the fact/implementation line, bulk import (T-557, 2026-09-22)

**Status:** PROVISIONAL, design only, no code and no template files. Use cases: **SIGNAL-049**
(ERT meters), **SIGNAL-053** (LoRa), **RESEARCH-002** (flex decoder for a never-seen sensor).
Source: [docs/18 §0, §3 Tier 1, §6](../18-decoder-coverage.md), which rank the short-range ISM long
tail as the highest-return coverage and call it "templates, not code". §§1–14 stand, except for the
deltas in §15.8.

**What binds, and what doesn't.** The only licence rule is the existing one in
[ADR-0010](0010-language-and-licence-ledger.md) and [ADR-0003](0003-process-plugin-model.md): GPLv3
decoder *code* stays behind the process boundary and nothing is derived from it in-core. The user
said on 2026-09-20 (T-555) that there is **no separate licence rule**. docs/18 §4's three-tier
proposal was cancelled, and this section neither cites nor revives it. What follows is a data
schema and a bookkeeping discipline. It adds no new gate.

### 15.1 Why templates are the bridge

A **template is data about a protocol** and a **block is code**. The licence question is only ever
about code. A template holds the same things every GNU Radio or rtl_433 decoder holds, and every
specification those decoders were written from: modulation family, symbol rate, sync word, check
polynomial, field layout and expected band. These are **protocol facts**. The schema has no place
for the rest of a decoder, which is its loops, taps, thresholds and state machines (§15.3).

The costs differ by orders of magnitude. A template takes hours. A block takes days. A wrapped
plugin is a dependency forever. For every protocol whose structure the ADR-0011 catalogue can
already express (docs/18 §3 Tier 1: OOK/ASK/FSK with PWM, PPM or Manchester coding and a CRC),
coverage therefore becomes data entry. And every template is also a MAUTO search seed (§4.2).

### 15.2 The fact-source field

§4.1's `provenance.kind` (`builtin | user | discovered`) says **who authored the template**. It
does not change, and ADR-0022 §5.1's template-fixed rule still reads it. This amendment adds a
second, independent record: **where each fact came from**.

```jsonc
"provenance": {
  "kind": "builtin",
  "facts": [
    { "fields": ["priors.families", "priors.symbol_rate_bd", "evidence_targets.S4"],
      "basis": "spec",                       // spec | tolerance | measured   (§15.3)
      "source": { "kind": "standard",       // see the table below
                  "ref": "ITU-R M.584-2", "locator": "Annex 1 §4", "accessed": "2026-09-22" } },
    { "fields": ["free[clock].domain"],
      "basis": "tolerance",
      "source": { "kind": "decoder-source", "project": "rtl_433", "artefact": "code",
                  "path": "src/devices/<file>.c", "commit": "<sha>",
                  "licence": "GPL-2.0-or-later", "licence_read_from": "COPYING" } } ] }
```

| `source.kind` | Meaning | Required keys |
|---|---|---|
| `standard` | A standards body's document (ITU, ETSI, IEEE, CCSDS, EN, ANSI, …) | `ref`, `locator` |
| `specification` | A published vendor or alliance spec, application note or datasheet (LoRa Alliance, Semtech, TI, …) | `ref`, `locator` |
| `paper` | A peer-reviewed or preprint paper | `ref` (DOI or arXiv id) |
| `reverse-engineering` | A published write-up of a protocol someone decoded (blog, talk, notes) | `ref` (URL) |
| `wiki` | A community wiki entry (sigidwiki, …) | `ref` (URL), `licence` |
| `decoder-source` | **Read from a decoder's own repository**: its code, tests, docs or config | `project`, `artefact` (`code \| tests \| docs \| conf`), `path`, `commit`, `licence`, `licence_read_from` |
| `measured` | Derived by this system from a capture: a discovered template, or a fixture fit | `job_id` or `fixture` (path + content hash) |
| `user` | The user stated it, with no further source | — |

Rules:
- **Granularity is the field path, not the template.** One template routinely mixes sources: a sync
  word from a standard, a rate tolerance from a decoder. Every fact-bearing field of a **builtin**
  template has to be covered by some `facts` entry. The loader refuses an uncovered field
  (`fact_unsourced`) the same way `Recipe::validate` refuses a dangling port.
- **Defaults fill themselves in, so the rule costs the user nothing.** A user-authored template's
  uncovered fields default to `{kind: user}`. A discovered template (§4.3) is stamped
  `{kind: measured, job_id}` on every field it fixed or narrowed.
- **`decoder-source` is recorded, not refused.** A template whose parameters came from reading a
  decoder's source must say so. That is the whole obligation. `licence` is read from the file
  itself, never from a forge API, because docs/18 §1.5 found GitHub's licence field wrong for
  several GNU Radio modules. A repository with no licence file records `licence: "none-stated"`.
  This is bookkeeping in ADR-0010's ledger sense, not a gate (CLAUDE.md).
- **Prefer the upstream description when one exists.** When a fact is available from both a
  `standard`/`specification`/`paper` and a `decoder-source`, the template cites the former. A
  template library lint lists fields sourced *only* from `decoder-source` (`resource_wanted`) so a
  later pass can re-source them. That list is information, not a failure.
- **No prose crosses.** `name` and `description` are written fresh. A wiki's or decoder's text is
  never pasted in, because text is expression even when the numbers beside it are facts.

### 15.3 The line: fact versus implementation, per §4.1 field

A **fact** is a statement about the air interface or the message format that two independent,
interoperable implementations must agree on. If a transmitter could change it and still be heard by
every existing receiver, it is not a fact about the protocol. **Implementation** is everything a
receiver's author chose in order to receive well.

| §4.1 field | Fact side | Implementation side | The awkward middle and its rule |
|---|---|---|---|
| `recipe` / `skeleton` | The protocol's **layering**: line code (NRZ, NRZI, Manchester, PWM, PPM), whether whitening is applied, where the check sits | Any other decoder's flowgraph or file decomposition. A skeleton is **always** expressed in ADR-0011's own blocks, never transcribed from someone's graph | A layer the catalogue lacks (`css_demod` for LoRa, docs/18 §7 rank 9) makes the template **inert** (§15.6), not a reason to copy a block |
| Node params the template fixes | Protocol parameters: `sync_word`, CRC RevEng model (`width, poly, init, refin, refout, xorout`), BCH code, whitening polynomial and seed, deviation, bit order, frame length | Loop bandwidths, filter taps and lengths, AGC constants, slicer thresholds and hysteresis, lock/unlock run lengths, timeouts, retry logic, any decoder state machine | **The schema enforces this line.** ADR-0011's `ParamSchema` gains `class: protocol \| tuning` (§15.8). A template may fix, range or seed only `protocol` params. `tuning` params keep the block's own defaults and are refined from the processed output (§2.3, T-070). No field exists to carry them, so a template *cannot* import them |
| `free[].domain` | An `enum` of values the spec lists (POCSAG 512/1200/2400) | — | **Ranges are the middle.** See the tolerance rule below |
| `priors.families`, `bursty` | Modulation family and duty cycle as the protocol defines them | — | — |
| `priors.symbol_rate_bd` | The nominal rate | — | The width of the range is a tolerance: tolerance rule |
| `priors.bandwidth_hz` | An emission mask or channel spacing the spec states | A decoder's channel-filter width | With no mask in the spec, the range is `basis: measured` from a fixture, or derived from the rate and family by `hk-synth` |
| `priors.bands_hz` | Allocations and spec channel plans | — | Rank only, for an emitter already detected there (§4.1). A wrong band costs nothing but order |
| `evidence_targets` | Sync length in bits; check kind and width | A decoder's "accept after N matches" | — |
| `plausibility` | Field widths and enumerations from the message format (a 21-bit RIC ranges over 0…2²¹−1) | A decoder's sanity filters ("drop temperatures above 70 °C") | A tighter "values actually seen" range counts as `basis: measured` and needs a fixture or job reference. A decoder's filter never passes as a spec fact |
| `output_policy` | — | — | Project policy, never sourced externally, so it carries no `facts` entry |

**The tolerance rule (the awkward middle).** Someone chose a symbol-rate window of ±3 %. That
number is not a spec constant, but it is not an implementation secret either, because it encodes
what that author saw real devices do. The rule treats it as **`basis: tolerance`**, which may come
from any recorded source, with three properties that make its origin low-stakes:
1. **A range only changes search order and budget, never a claim.** A range that is too wide costs
   evaluations. A range that is too narrow costs a miss, and then the open-search floor (§4.2, ≥ 20 %
   of budget) still runs. Continuous-parameter ranges do not enter the confirm gate at all: ADR-0022
   §2.1 pays only analytic check bits, net of `L_check`.
2. **The engine never narrows below measurement.** The effective domain is
   `range ∪ (estimate ± k·σ_estimate)`, so a blind estimate outside a template's tolerance is still
   searched. The template ranks it lower (a prior), and nothing vetoes it (ADR-0016's rule).
3. **With a spec tolerance, use it. With none, derive the tolerance, don't copy it.** Prefer the
   spec's own figure (`basis: spec`). Otherwise use `hk-synth`'s default widening by timing class,
   `timing: crystal | rc | unknown`, which the template declares as a fact about the transmitter
   (for example ±2 % / ±25 % / ±50 % — **initial guesses, unverified**). A copied decoder window is
   permitted, recorded as `basis: tolerance, source: decoder-source`, and listed by the
   `resource_wanted` lint.

### 15.4 What a template can and cannot do, and the one exposure bulk import creates

These safeguards make it safe to import a template from a reference decoder, which would not be
true of importing the decoder itself. Each restates an existing rule:
- **Priors order the search. They NEVER rank a result and NEVER confirm one** (§1.3, §4.1).
  `prior_bits` is kept separate from `evidence_bits` and is reported but never used as a key.
- **A template can never confirm a signal on its own** (§4.1). Confirmation needs measured hold-out
  frames that pass a check, under ADR-0022's inequality.
- **`bands_hz` only raises rank for an emitter already detected there** (§4.1). A template never
  tunes, never creates a candidate and never pre-populates the inventory (CLAUDE.md, workflow #4).
- **A template cannot starve unknowns**: open-search floor and defer-don't-delete (§4.2).
- **A validated template earns nothing extra** (§15.6). Validation is quality control on the
  library. It is never a bit source.

A template imported from a reference decoder therefore cannot make the system claim something it
did not measure. The worst case for a wrong template is wasted budget, or a decode label that has
to pass a real check on real frames.

**The exposure, stated rather than hidden.** ADR-0022 §5.1 sets `L_check = 0` for a template-fixed
check "because zero hypotheses were tried". That holds when **one** template is tried against a
window. It stops holding when a library of hundreds is tried. If a job tries *N* template-fixed
checks against the same window, that is *N* chances for noise to pass one of them, and ADR-0022
§2.3's own attribution test ("the trials that could have produced *this* fit") counts every one of
them. ADR-0022 §4.2 already applies this to one template tried at several framings. The same logic
applies across templates:

> **`L_check` for a template-fixed check is `log2(number of distinct template-fixed check
> hypotheses evaluated against that window's frames at the check stage in this job)`.** It is zero
> only when a single template's check was tried.

Worked case: 380 ISM templates, each with a CRC-8, all tried on one burst, give
`L_check = log2 380 ≈ 8.6 bits`, which is more than the check's own 8. ADR-0022's
`min_differences = ceil((24 + 8.6) / 8) = 5` differing frames are then needed instead of 3. With a
confident classification, priors put a handful of templates first, the job stops early, and the
charge is small. The charge counts **what was tried, not the library's size**. So a large library
costs confirmations only when the search actually had to spread across it, which is correct.
Without this rule, bulk import would be the one way a template *could* help cause a false confirm.
This delta belongs to ADR-0022 and is handed to **T-575**, which applies that ADR (§15.8).

### 15.5 The bulk path: hand-authored, spec-first, one skeleton at a time

There are three candidate paths. The position is: **(a) and (c) yes; (b) no mechanical generator
from any decoder corpus; one narrow format importer.**

**(a) Hand-authored, batched by skeleton.** One skeleton (`generic-ook-pwm`,
`generic-ook-manchester`, `generic-fsk-framed`) carries many parameter sets. Authoring a template is
then filling in about a dozen facts with sources, in the order docs/18 §3 ranks use cases. This is
the primary path. rtl_433's device list is used as an **index** of which protocols exist, in which
bands and under which modulation class, so authoring can be prioritised. The list itself is not
copied into the repo as a table.

**(b) Generated from a structured corpus: no.** The reason is not that templates are a licence
problem, because they are data. The reasons are these:
- **rtl_433's facts live in code, not data.** Its ~380 decoders are C (`src/devices/*.c`). The
  `r_device` initialisers hold pulse timings, but the sync match, CRC call and field extraction are
  inside decode functions. A generator would be a C parser for arbitrary decode functions: brittle,
  and most of its output would fall on the implementation side of §15.3 (`gap_limit`,
  `reset_limit`, and the decoder's own acceptance logic).
- **A generator ships hundreds of unvalidated claims in one commit.** Each one costs budget and
  §15.4 multiplicity. The library's value grows with *validated* templates, not with its size
  (§15.6).

**The one structured translator worth building is for RESEARCH-002.** It is an importer for the
**rtl_433 flex (`-X`) spec language**, a small declarative format (`modulation`, `short`, `long`,
`gap`, `reset`, `preamble`/`match`, `bits`, `repeats`). It maps directly onto the generic OOK
skeletons and applies §15.3 as it translates:
- `modulation` becomes the skeleton;
- `short` and `long` become `priors` with `basis: measured` (someone measured those pulses) and a
  derived tolerance;
- `preamble`/`match` becomes `evidence_targets.S4`;
- `gap` and `reset` are **dropped**, because they are receiver limits and the burst detector
  (T-075) derives them.

A spec string the user wrote is `kind: user`. That is RESEARCH-002 as the product sees it: describe
a never-seen sensor's timing and get a searchable template, not a hard-wired decoder. One of
rtl_433's shipped `conf/*.conf` files can go through the same importer **one file at a time, on
request**, and is stamped `decoder-source` (artefact `conf`, commit, `GPL-2.0-or-later`)
automatically.

**(c) Grown by the user through save-as-template (§4.3).** This path is already safe:
`kind: discovered` inherits the discovering search's look-elsewhere (ADR-0022 §5.1), and every field
it fixes is stamped `measured`.

**Corpus licences (ADR-0010's ledger rule).** A ledger row is added by the ticket that first *uses*
a corpus as a fact source, like the gpsjam data-source row. None is adopted by this amendment.

| Corpus | Licence | Status | Proposed use |
|---|---|---|---|
| rtl_433 (code, `conf/`, docs) | GPL-2.0-or-later | In the ADR-0010 ledger (subprocess plugin row) | Index for prioritising; per-field `decoder-source`; per-file flex import. **No bulk generator.** The plugin stays the long-tail escape |
| rtl_433_tests (sample `.cu8` captures) | **Unverified**: read the repo's licence file before use | Not adopted | Candidate **validation fixtures** (§15.6). Nothing enters `fixtures/` until the licence is read from the file |
| sigidwiki.com | **Unverified**: read the site's content licence and terms before use | Not adopted | Facts only (frequency, mode, bandwidth, baud), at S0–S2 depth. Better suited to the explanation/recommendation database than to decode templates, because it rarely carries field maps or checks. No scraping; no prose |
| CRC RevEng catalogue | Parameter facts; already used (T-013 ledger row: "parameters only, no code copied") | Precedent | The `crc` model and its `check` value (§15.6) |
| rtlamr (SIGNAL-049) | AGPL-3.0 (**verify** from the file) | Not adopted | `decoder-source` (artefact `docs`) for the SCM/IDM formats, pending a better public description |
| gr-lora_sdr (SIGNAL-053) | GPL-3.0 | Not used | Not needed: the LoRa PHY is described in a paper and vendor application notes (§15.7) |
| Standards bodies (ITU-R, ETSI, CCSDS; IEEE where accessible) | Each document's own terms; facts only | — | The preferred `standard` source |

### 15.6 The test: templates are claims about the world and can be wrong

The design already refuses to trust a template: nothing a template says ranks or confirms anything
(§15.4). Validation is therefore **quality control on the library, never trust**. A template's
parameters are a claim, and there are four levels of checking them, which report honestly what
each one proves:

```jsonc
"validation": [
  { "level": "consistency" },                                              // loader, always
  { "level": "synthetic", "generator": "hkpy.synth:<name>", "commit": "…" },
  { "level": "fixture", "fixture": "fixtures/…sigmf-meta", "sha256": "…", "use_case": "SIGNAL-049",
    "result": { "rank": 1, "stage": "S5", "differences": 7 } },
  { "level": "field", "job_id": "…", "emitter_id": "…" } ]
```

1. **`consistency`: every load, free.** The template has to be internally coherent. The CRC model
   includes the RevEng `check` value (the CRC of `"123456789"`), and the loader verifies that
   `hk_estimate::framing::crc` reproduces it, which catches a mistyped polynomial, init or reflect
   flag at load. Sync length matches `evidence_targets.S4.sync_bits`. Field-map widths sum to the
   frame length. Every `free` path names a `protocol`-class param (§15.3). Every fact field is
   sourced (§15.2). Failure is a load error for builtins and a validation error for user templates.
2. **`synthetic`: proves expressibility, not truth.** A synthetic generator built from the template's
   own facts shows that ADR-0011's blocks can express and decode the signal. **It is circular about
   the facts**, because a wrong sync word produces a synthetic with the same wrong sync word, so it
   never counts as evidence that the facts are true.
3. **`fixture`: the real test.** A real capture (own SigMF, or a licence-checked external one) is
   run **blind through the mock SDR** as a §7 evaluation, with templates on and templates off
   against a hidden truth list. It asserts the template's hypothesis is the rank-1 solved result and
   the decode passes its check. Only this level, or `field`, validates facts. Builtins with a
   fixture run in the T3 tier. Use-case IDs key the fixtures, per CLAUDE.md.
4. **`field`: accrued, never asserted.** A confirmed emitter whose winning pipeline came from this
   template is recorded by `job_id`/`emitter_id`.

Beyond the four levels:
- **Unvalidated templates may ship, and are listed as such.** A `consistency`-only builtin is legal.
  It costs budget and §15.4 multiplicity, which is the real price of an untested claim. A builtin
  whose skeleton names a block the catalogue lacks is **`inert`**: it loads, validates and is
  listed, but is never seeded. The analyze trace reports it as ADR-0021's
  `missing_block` with `suspected_by: template`.
- **A wrong template surfaces from its record.** ADR-0021's `ruled_out` trace already records
  "template X tried, deepest stage S2". A lint reports templates that were tried on k or more
  emitters inside their own priors and never reached S5, as a prompt for review. It never
  auto-deletes, never demotes, and never changes a prior. Removing or fixing a template is a human
  act and a new version (§4.1 immutability).

### 15.7 Worked sketches (illustrative; every number below is unverified)

- **RESEARCH-002, a never-seen 433 MHz sensor.** The user pastes
  `-X "n=probe,m=OOK_PWM,s=500,l=1000,r=4000,bits>=36"`. The importer yields `generic-ook-pwm` with
  pulse-width priors {500, 1000} µs (`basis: measured`, source `user`), `bits ≥ 36` as a
  plausibility on frame length, and `r` dropped. There is no check, so the template can order the
  search but can never contribute analytic check bits. Confirmation, if it ever comes, needs a check
  the search *finds*, charged by ADR-0022 §5.1. That is the honest outcome for a sensor nobody has
  specified.
- **SIGNAL-049, ERT SCM.** Manchester OOK, a fixed preamble and a 16-bit BCH-style check. The best
  available public description is rtlamr's own protocol notes, so the facts are
  `decoder-source {project: rtlamr, artefact: docs, licence: AGPL-3.0 (verify)}`, and the
  `resource_wanted` lint lists them. The template is fully expressible in today's catalogue
  (Manchester, `sync_search`, `crc`) and needs a `fixture` validation from the user's own meter
  capture before anyone relies on it.
- **SIGNAL-053, LoRa.** SF 7–12, BW 125/250/500 kHz, sync word and CRC-16 are facts sourced from
  `paper` (Tapparel et al., the open LoRa PHY paper) and `specification` (Semtech application notes,
  the LoRa Alliance LoRaWAN spec for the MAC headers SIGNAL-053 reads: DevAddr, FCnt). The GPLv3
  `gr-lora_sdr` is not needed as a source. The skeleton needs `css_demod` and `descramble`
  (docs/18 §7 ranks 9 and 2), so the template ships `inert` until those blocks land. Once they do,
  it becomes live with no template change.

### 15.8 Deltas

- **§4.1 schema:** `provenance.facts[]` (§15.2), `validation[]` (§15.6), and a `timing` class for
  the tolerance rule (§15.3). The `inert` state is derived from the catalogue, not stored.
  `hackriff.template` stays `schema_version: 1` because nothing has been implemented yet. If
  implementation lands first, these fields are `2`.
- **§4.3 save-as-template:** stamps `facts: [{fields: <all fixed/narrowed>, basis: measured,
  source: {kind: measured, job_id}}]`. The ADR-0022 §5.1 inherited-L field is unchanged.
- **ADR-0011 `ParamSchema`:** gains `class: protocol | tuning`. `tuning` params are neither
  template-fixable nor template-seedable. This is a descriptor contract change, listed here for
  whichever ticket next amends ADR-0011 (docs/18 §8's T-606 carries catalogue deltas already), and
  **not applied by this amendment**.
- **ADR-0022 §5.1:** `L_check` for template-fixed checks counts the template-fixed check hypotheses
  tried against the window (§15.4). **For T-575**, which applies ADR-0022. It is not applied here.
- **ADR-0010 ledger:** a corpus row when a corpus is first used as a fact source (§15.5).
- **Unchanged:** §§1–3, §§5–14, and ADR-0022's confirm inequality apart from the `L_check`
  counting above.

*Unverified in this amendment: the tolerance-class widenings (±2/25/50 %); the licences of
rtl_433_tests, sigidwiki and rtlamr (to be read from their files); the §15.7 protocol constants;
the rtl_433 decoder and flex-conf counts; and whether a handful of templates really suffices on a
well-classified burst for §15.4's charge to stay small. That last point is measured by the templates-on
runs in §7.*

---

## 16. Acceptance review (T-848 = MAUTO M-1, 2026-09-23) — **ACCEPTED 2026-09-23 (user decisions U1–U5)**

**Status of this section:** the review T-848 prepared, and the record of its acceptance. The user
answered docs/20's five questions on 2026-09-23 (relayed by the supervisor) and said "accept
ADR-0015 accordingly, and let the MAUTO M-1/M-3 chain proceed". §16.2's corrections are now folded
into the sections they amend; §16.3 records the decisions; **§16.8 lists what the acceptance did
not decide — those stay open and are not resolved by it.** The M-1 scaffold the review was checked
against is the `hk-synth` crate plus `hk_model::synth`.

### 16.1 What was checked, and against what

Every contract type ADR-0015 and its amendments name was written down as Rust and tested for
shape: `Stage`, `MetricId`, `GroupId`, `Evidence`, `EvidenceSet` (§1.1, §2.1, §13.1);
`NodeScore`, `prior_bits`, the look-elsewhere charge, default caps and floors (§1.3, §13.2);
`Score` / `Threshold` / `Unexpressible` and the fill buckets (§13.2–§13.3); `Candidate`,
`FreeParam`, `Domain`, `SeedSource` (§1.2); `Skeleton` (§1.2); `Template` with §15's fact
provenance and validation (§4.1, §15); `ProposalOp` (§3.2); `Profile`, `SynthBudget`,
`StopReason`, `JobState`, `NodeHeuristic` (§3.3, §5.2); `Verdict`, `PipelineResult` (§3.4, with
ADR-0022's `analytic_holdout_bits` and ADR-0021's `characterisation`); and ADR-0021's
`TraceNode`, `Outcome`, `Resolution`, `Reason` (ADR-0021 §12's M-1 amendment). A candidate prefix
validates against the real `hk_blocks::Registry::builtin()` catalogue
(`crates/hk-synth/tests/candidate_is_a_recipe.rs`), so "a candidate is a recipe" is checked, not
asserted. **No engine behaviour exists**: M-2…M-12 fill the modules, each named in
`crates/hk-synth/src/lib.rs`.

### 16.2 Engineering corrections — folded into the normative text at acceptance

| # | Finding | Now stated in |
|---|---|---|
| **D1** | `Evidence`/`EvidenceSet`/`Stage` must be nameable by `hk-blocks` (`Block::evidence`, M-2), but §9 had `hk-synth` depend on `hk-blocks` — a dependency cycle. The vocabulary lives in **`hk_model::synth`**; `hk-synth` re-exports it. | §9 (crate table + the paragraph under it) |
| **C2** | §4.2's "posterior < 0.02 is deferred" contradicted ADR-0016 §8 / T-215: only the **likelihood** defers; a prior reorders. `hk_synth::seed` defers exactly when `Hypothesis::prune` says so (test: posterior 0.01, likelihood 0.3 stays active). | §4.2 "Defer, don't delete" |
| **C3** | ADR-0021 §1 rule 3 (a memoised hit carries no measurement) vs §2.2 (every tried node's `measured` is non-null). `TraceNode::check` requires a measurement on every tried node **except** `memoised`. | ADR-0021 §2.2 (note added) |
| **C4** | §4.1's `output_policy.metadata_keys` list vs the recipe's typed **map**. `Template.output_policy` is `hk_recipe::OutputPolicy`; the list is shorthand. | §4.1 |
| **C5** | "There is no `psk_demod`" was stale: T-609 landed it. | §1.1 catalogue gap; §10 M-14 row |
| **C6** | §11.7's "new §2.28 CandidatePipeline": docs/07 §2.28 is `TrunkSystem` (T-266). CP-1 takes the next free number (§2.33 today). | §11.7 |
| **C7** | S6 had no stated floor. `default_floor_bits(S6) = None`: S6 ranks only. | §1.3 Floors |
| **C8** | `Threshold` snaps **up** and refuses; a floor above `admissible_bits` is `floor_unreachable`, never lowered. | §1.3 Floors |

### 16.3 The user's decisions on docs/20 (2026-09-23)

| # | Question | Decision (user 2026-09-23) | Applied in |
|---|---|---|---|
| **U1** | Does one clean burst confirm a signal? | **B** (the recommendation): auto-confirm on one frame only when the check was **template-fixed and ≥ 24 bits**; searched checks never. False-confirm budget **≤ 1 wrong Confirmed emitter per unattended week**. ADR-0022 already realises this as one inequality (ADR-0022 §4.2), so no extra predicate is added. | Decision summary "Bursts"; §5.5; §6 |
| **U2** | May the device analyze on its own; may `deep` run on battery? | **B** (the recommendation): auto-queue unknown candidates at **`quick` only, mains only, one job at a time**; `standard`/`deep` user-triggered; `deep` refused on battery. `auto_profile: quick \| none`, product default `quick`. | §3.3 "Auto-analyze"; §8 |
| **U3** | User label vs a CRC-valid decode that disagrees | **A** (the recommendation): **the user wins (rank 0)**; the decode is recorded and shown beside the label. Closes ADR-0016 open question 3. | §4.2 feedback; §5.5; §11.10 Q5; ADR-0016 Q3 |
| **U4** | Fund PSK / SSB / CW blocks? | **A** (the recommendation): **fund `psk_demod`; M-14 is required** — its block is delivered by T-609 (residue in §10's M-14 row). **SSB/CW stay on the legacy Listen chain permanently.** | §1.1; §10 M-14; §12.5; §12.10 item 5; §12.12 |
| **U5** | Stereo audio? | **Yes — overriding the brief's recommendation ("no").** Stereo is part of decoding the signal and stays in the ADR. | §12.10 item 6; §12.12 Q4; **§12.13** (LP-9, LP-10) |

The two numbers docs/20 sent to measurement stay provisional **by design**: the 4-bit supersession
margin / 0.6 overlap (T-547 — still first guesses for §11.3) and the `quick`/`standard`/`deep`
budgets (T-552 — §16.8 item 2). §7's thresholds are "fixed before implementation" and may only
tighten.

### 16.4 The fill-bucket amendment T-619 triggered — **still pending** (not decided at acceptance)

**§13.3's single `nominal` bucket is falsified on paper.** §13.5 item 4 named its own trigger, and
**T-619 fired it** (docs/21 §10): the AM/OOK metrics over-claim 5–6 bits at high clip, the measured
safe region is σ ≥ 1.0 LSB **and** clip ≤ 10 %, and the runtime rule must read the **noise
floor's** fill, not the window's. The acceptance did not take this amendment. **T-660** (done)
deliberately did not either: `hk_synth::calibration::FillBucket` and `hk_model::provenance::FillBucket`
both keep §13.3's as-written bounds (0.5 / 0.30) so the two crates stay in step, and the py
generator takes the bounds as a parameter (`ADR_NOMINAL_BOUNDS` default, `TIGHT_NOMINAL_BOUNDS`
available). **Standing constraint:** the amendment must be written, or M-2's first real calibration
generation must pass `TIGHT_NOMINAL_BOUNDS` explicitly, **before any calibration table is generated**
— no table exists yet, so nothing is invalidated by the deferral. §16.8 item 3.

### 16.5 The ADR-0016 dependency — waived by the acceptance

§10 made M-1 depend on "ADR-0016 accepted". ADR-0016 is **PROVISIONAL** with open questions 1
(M3 exit floors / OTA captures), 2 (tract in the default build), 3 (= U3, now answered), 4 (cluster
novelty → ADR-0012) and 5 (answered by docs/20 D1: local only). T-206's gate result also records M3
**not closing** on two blockers (unknown recall 0.778 vs 0.80; no end-to-end classification row).

What `hk-synth` consumes from ADR-0016 is **§8's `SearchSeed` only**, which is landed code (T-215,
`hk_model::classify::seed`) read by one adapter (`hk_synth::seed`). Priors only order the search, so
M3's accuracy floors cannot change any hk-synth contract — a weaker classifier costs search budget,
not correctness. The ask (§16.7) bundled this waiver with acceptance and ADR-0016 was not accepted,
so **accepting ADR-0015 waives the dependency**; ADR-0016's own review (Q1, Q2, Q4) stays separate.

### 16.6 Not blocking, listed for completeness

- ADR-0021's five open questions (false-label budget, null-control cost, 8-bit null margin, trace
  retention, auto-retry) and ADR-0022's Q1 are parameters of M-3/M-9/M-12, not of the contracts
  M-1 fixes. They stay with those ADRs.
- The §9 deltas (docs/07 §2.11 `synthesis`, §2.15 Decode `provenance`, a Template object; docs/api
  "Analyze"; stream `hackriff.analyze/1`; recipe `schema_version` 3) are still unwritten; each lands
  with the ticket that first serves or stores it (M-8, M-9, M-7, CP-1), per the T-079 rule. U5 adds
  one: the `audio` stream's `channels` field and version bump (LP-10).

### 16.7 The ask (as put to the user; answered 2026-09-23)

> **"Accept ADR-0015"** — with U2, U3 and U5 answered (or "take the recommendations"), SSB/CW as
> proposed, §16.4's amendment deferred to T-660, and the ADR-0016 dependency **waived** (or ADR-0016
> accepted). On that word the status line changes to ACCEPTED and §16.2's corrections are folded
> into the sections they amend. Until then M-2…M-7 can start against the scaffold: none of them
> needs an answer above except M-3's `auto_profile` default, which starts off.

**Answer (user, 2026-09-23):** U1 B, U2 B, U3 A, U4 A (M-14 required), U5 **yes** (against the
recommendation); accept ADR-0015 accordingly; the MAUTO M-1/M-3 chain proceeds.

### 16.8 Still open after acceptance (the user did not decide these; the acceptance does not resolve them)

1. **S3 cannot reach `floor_j = 6` under the maximum rule (T-660).** Combining docs/21 §2's published
   per-metric realised bits through §13.1's rule with the M1 FSK ladder's declared groups: S2 goes
   17.1 → 8.2 bits and still clears 6; **S3 goes 8.8 → 4.5 bits and does not** — S3 has one declared
   group (`bit_shape{line_violations, bit_structure}`), so `b_3` is its best single metric, and
   neither clears 6 alone. §13.1 predicted this in words. The rule stands: **restate S3's floor
   (T-660 suggests roughly 4–5 bits) beside that measurement, never return to the sum.** Until an
   amendment does, `hk_synth::stage::default_floor_bits` keeps 6 (documented there), and §1.3's
   "pruning is never total" means the cost is search recall, not silent loss. Needs a floor
   amendment; not decided here.
2. **The budget unit (T-552, docs/27 — in flight, not yet on `main`).** Per-stage DSP is cheap and
   dominated by S0 channelisation (memoisation amortises it), but **proposal-operator calls cost
   0.3–2.4 s each** and dominate wall time, so a `wall_s` budget buys wildly different amounts of
   search. docs/27 §6 recommends §3.3's 3 / 20 / 120 s wall budgets become **operation/evaluation-count
   budgets** (e.g. `max_proposal_calls` or a shared `assist::Budget{max_ops}`), with **wall time kept
   only as a backstop** for the API and power story, and `quick`'s 3 s re-examined before it ships in
   the public API. Not decided here; M-3 should not freeze `SynthBudget`'s public shape before it is.
3. **The fill-bucket amendment (T-619 → §16.4).** Pending; binds M-2's first calibration generation.
