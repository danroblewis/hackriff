# ADR-0015 — Decoder synthesis contracts: candidate pipelines, stage evidence, search, templates, region analyze

**Status:** PROVISIONAL (T-208, core interface, planning only). MAUTO is unscheduled until after M3. No code comes from this ADR until then.
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
| Bursts | Burst sets from the ring (ADR-0014), joined with `DISCONTINUITY`. Pin-on-analyze clip. No single-frame auto-confirm by default. |
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

**Catalogue gap:** there is no `psk_demod`/Costas block, so generic PSK stops at S1 until one is added. That addition is an additive ADR-0011 §1.5 change (MAUTO task M-14, optional).

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
  - `b_j` is the stage-j significance (§2);
  - caps default to 12 bits for S0–S3, 32 for S4, and no cap for S5–S6;
  - `L_j = log₂(hypotheses evaluated at stage j in this job)`: the look-elsewhere cost, charged by the engine.
- `prior_bits = log₂ π(h)`, clipped to [−8, 0] (§4.2).
- The **search order** key is `evidence_bits + prior_bits + optimistic_remaining`.
- The **result rank** key is `(stage_reached, evidence_bits)`. Prior bits are reported but never rank a result and never confirm one.
- **Floors** prune: a node whose stage-j `b_j` < `floor_j` (default 6 bits S0–S3, 10 bits S4–S5) is pruned. Pruning is never total: the best pruned node is kept as a partial result.
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
  - Whether these tails are stable enough across 8-bit quantisation and gain states is **unverified**; M-2 measures it.
- The engine, not the block, subtracts the look-elsewhere cost, because blocks don't know how many hypotheses ran.

### 2.3 Refinement reuse (generalising T-070)

`hk_synth::EvidenceObjective` implements `hk_demod::refine::Objective`:
- `space()` maps the candidate's continuous free parameters to `ParameterSpace` centre and bandwidth plus `Tuning.mode` axes (deviation, symbol rate, loop bandwidth) by name.
- `evaluate(window, tuning, depth)` runs the prefix and returns `Measurement { quality: evidence_bits, locked: deepest b_k ≥ floor_k }`.
- `EvalDepth::{Acquire, Track, Validate}` map to the short, search and hold-out windows.

`RefinementLoop`, `Termination` and hysteresis are reused unchanged. The WFM objective stays as the specialised S1 evidence for broadcast FM.

Recipe `refine.objective` gains `{"evidence": "deepest"}`. That is a new optional key, so `schema_version` 3 (ADR-0011 §2.4 rule). A running synthesized pipeline then keeps tuning from the same evidence.

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

The numbers are **unverified guesses**, measured in M-3 on the Mac and later on the Jetson.
- **Chain kind `synth`** in the per-run chain budget (ADR-0011 §1.4 rule 5). Search threads run at lower OS priority than ring readers.
  - If the run's `lost_samples` rises while a job runs, the job throttles (`state: throttled`, threads halved) before capture is hurt.
- **One running job** by default and a queue of ≤ 4. Beyond that: `503 busy`.
- **Power.** The job reads the run's power policy (ADR-0007/0009): `battery` refuses `deep` (`422 power`) and halves threads, `low` allows only `quick`, and a thermal-throttle flag pauses expansion. (No such policy input exists in code yet; M-3 adds a minimal one.)
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

### 4.2 Seeding and pruning from M3 (MAUTO side of ADR-0016)

MAUTO reads, and does not define, these fields of ADR-0016's types. If ADR-0016 names them differently, one adapter in `hk-synth::seed` absorbs the difference.
- `Classification`: `families{name: posterior}` including `unknown`, `open_set_score`, `taxonomy@version`, provenance.
- `SignatureMatch`: `kind (full | partial | none)`, `candidates[{signature_id, version, score, per_field, pipeline_binding}]`, `cluster_id`.

The prior for hypothesis h is:

`π(h) = P_class(family(h)) × match(h | measured params) × band_factor(h)`, with `band_factor` ∈ [1, 1.5].

**Rules:**
- **Open-search floor.** Open skeletons always get ≥ max(P(unknown), 0.2) of the evaluation budget. Priors never starve unknowns.
- **Defer, don't delete.** A family with posterior < 0.02 is deferred: it runs after higher-ranked hypotheses, if budget remains. Evidence found under a deferred family ranks exactly like any other.
- **Signature fast path:**
  - a `full` match with a pipeline binding is tried first, at the signature's parameter values;
  - a `partial` match narrows its template's free ranges to the signature tolerances;
  - a known `cluster_id` warm-starts the beam from that cluster's last attached result (§5.5).
- **Feedback (writes, through M3 APIs only).** A solved result emits a decode label `{source: decode, family, template, job_id}` to C15 (a CRC-valid decode overrides classification, C15 card), proposes a C18 `Signature` from the solved parameters (provenance `decoder-confirmed`, T-201 route), and feeds its hold-out snippets to T-205's labelled-capture path.

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
- **Single frames** (§6) don't auto-confirm by default: the emitter stays a candidate with `verdict: solved` and the user can promote. (Open question 1.)

## 6. Burst and one-off path

- **When.** The target emitter's sightings are T-075 burst detections, or the window is ≤ 50 ms, or `source: ring` names a past instant.
- **Burst set.** The engine collects bursts in the window that belong to the target: same emitter, same C18 `cluster_id` when M3 provides one, or burst boxes overlapping the band.
  - Each burst is `[t_start − guard, t_end + guard]` (guard = max(2 ms, the burst duration)), read from the ring.
  - Bursts are concatenated with `DISCONTINUITY` between them, capped at 64 bursts or 2 s of IQ.
  - Evidence `n` accumulates across bursts, and hold-out is the odd bursts.
- **Pin on analyze.** At job start the acquired ranges are exported as a pinned clip (`Recording` kind `iq-snippet`, trigger `analyze`) before the search, so ring eviction can't race the job. The clip id is in `window`, and a re-run can target it.
- **Search changes.** The S0 seed is the burst's short-FFT centre (±1.2 kHz, T-075); there is no tracking refinement across time; S2 seeds come from the preamble (assist `periods`); `ppm_demod` skeletons are tried first for ms-scale bursts at ≥ 1 Msps.
- **One burst.** It can reach `solved` (e.g. one ADS-B frame passing CRC-24 under the `adsb` template). It attaches but does not auto-confirm (§5.5).

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
- **Automatic analysis of every detection.** v1 is user- or API-triggered. Queueing jobs from the ADR-0012 attention scheduler is a later policy (open question 3).
- **ML on the critical path** (§3.3 slot only).
- **Transmit or active probing.**

## 9. Crate placement

| Crate | Holds | Depends on |
|---|---|---|
| `hk-synth` (new) | `Stage`, `Evidence` scoring and nulls, candidate and skeleton types, search engine, proposal adapters, `EvidenceObjective`, template schema, loader and seeding | hk-recipe, hk-blocks, hk-estimate, hk-demod, hk-model |
| `hk-blocks` | `Block::evidence`, `EvidenceSet`, per-block evidence, the batch `run_window` driver | (existing) |
| `hk-pipeline::synth` | job manager, acquisition (ring/live/burst set, pin clip), `synth` chain kind, attach, the `ConfirmPolicy.synthesized` rule | + hk-synth |
| `hk-store` | `IqBufferService::read` backing (ADR-0014 format, read-only) | (existing) |
| `hk-model` | `emitter_synthesis` table, Decode provenance, template store paths | (existing) |
| `hk-api` (`analyze.rs`) | the routes in §5 | (existing) |

**Deltas to write when MAUTO is scheduled:** docs/07 §2.11 (`synthesis`), §2.15 (Decode `provenance`) and a new Template object; docs/api.md "Analyze" (replaces the T-190 section); stream-contract `hackriff.analyze/1`; recipe `schema_version` 3 (`refine.objective.evidence`).

## 10. MAUTO task graph (sketch; ids TBD; not in tasks.yaml)

Scheduled only after the M3 exit (T-206). There are no Fable tasks; core-interface tasks go to Opus and are reviewed before merge.

| Id | Task | Deps | Model | Group |
|---|---|---|---|---|
| M-1 | `hk-synth` scaffold: Stage/Evidence/candidate/skeleton/template types, pre-added modules and stubs, ADR-0015 → ACCEPTED review | T-206, ADR-0016 accepted | Opus (core) | SYN-0 |
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
| M-14 | Optional `psk_demod`/Costas block (ADR-0011 catalogue addition) | M-2 | Opus | SYN-B |

**Waves** (≤ 4 Rust builders at once): (1) M-1; (2) M-2, M-3, M-4, M-6, with M-5 once T-199/T-201 are done; (3) M-7, M-8, M-5; (4) M-9, M-10, M-11; (5) M-12, then M-13.

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

1. **Single-burst confirmation.** Should one frame passing a template-fixed ≥ 24-bit check (e.g. an ADS-B squitter) auto-confirm, or only attach, leaving promotion to you (proposed)?
2. **Budget defaults and battery.** Are `quick` 3 s / `standard` 20 s / `deep` 120 s right for a handheld? Should `deep` be refused on battery?
3. **Auto-analyze.** Should the attention scheduler eventually queue analyze jobs for unknown candidates by itself, or stay user-triggered only?
4. **Discovered templates.** Keep them as local JSON files (proposed; exportable), or plan a share/export format now?
5. **Generic PSK.** Include the `psk_demod`/Costas block (M-14) in MAUTO, or accept that PSK stops at `demodulated`?

## 11. Amendment — a candidate **is** a decode pipeline (T-220, 2026-09-15, from the user)

**Status:** PROVISIONAL, planning only, no code. Source: [docs/15 §10](../15-decoder-synthesis.md), written by the user after live testing. §§1–10 stand unchanged; this amendment changes *where the search's results live* and *what an inventory entry contains*. Task ids T-230+ are proposals for the coordinator, not entries in `tasks.yaml`.

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

**The constraint on T-219 so it doesn't fight this:** record grouping as **append-only rows keyed by emitter, carrying reason and score** — never by mutating or deleting the losing rows. A T-219 duplicate group then becomes, unchanged, the set of competing pipelines on one emitter (T-235).

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
  2. **MAUTO wave 0 (T-230):** tables and read path land **inert** — no writer, no behaviour change, nothing reads them for a decision.
  3. **T-232/T-233:** synthesis attaches pipelines; promotion drives `ConfirmPolicy`.
  4. **T-235:** T-219's groups migrate onto the object (append-only row migration, no semantics change).
  5. **T-221/T-236:** Listen becomes an audio-output pipeline. Listen is today its own chain type (`hk-pipeline/src/chains/listen.rs`, `ChainKind::Listen`) served through the generic on-demand opener (`OpenerRegistry::with("listen", …)`, `/ws/open/listen`), with **no** listen-specific hk-api route — so the opener name keeps working unchanged while the implementation moves onto a recipe.
  6. **T-237:** the UI shows competitors and evidence ladders.
- **Reversal cost** is low at every step: 2–4 are additive tables plus one nullable column; dropping the feature means ignoring the rows.

### 11.7 docs/07 delta (sketch, applied by the implementing task)

- **§2.11 Emitter:** an Emitter owns 0..N **candidate decode pipelines** (§2.28), ranked by evidence; its measured `f`/`BW` are never overwritten by a pipeline's channel; confirm-by-decode promotes a pipeline and confirms the emitter. Add the `duplicate_of` / `artifact_of` / `suppressed_by` links from §11.4.
- **New §2.28 CandidatePipeline:** the §11.1 object — identity, append-only event lifecycle, ranking and supersession, relation to Recipe/Classification/Decode, retention (kept with the emitter; superseded rows summarised, never deleted), and tests.
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

### 11.9 Task graph (proposed ids T-230+; the coordinator applies them)

| Id | Task | Deps | Model | Group |
|---|---|---|---|---|
| T-230 | `hk-model` candidate-pipeline types + migration 0008 + repo (append-only events, rank function), landed **inert** | T-219, this amendment reviewed | Opus, core_interface | CP-0 |
| T-231 | Ranking + supersession engine: same-hypothesis test, 4-bit margin, revive-on-new-evidence; property tests for reversibility | T-230 | Opus | CP-R |
| T-232 | Synthesis attach writes pipelines (replaces §5.4 attach-only); `decode.candidate_pipeline_id` | T-230, M-8, M-9 | Opus, core_interface | CP-W |
| T-233 | Promotion → `ConfirmPolicy` wiring: hold-out rule, corrected-group exclusion (T-210), one promoted row per `output_kind` | T-232 | Opus, core_interface | CP-W |
| T-234 | Routes (§11.8) + `docs/api.md` + contract tests | T-230 | Opus, core_interface | API |
| T-235 | Fold T-219's duplicate groups, suppressions and artifact links onto the pipeline object | T-219, T-231 | Sonnet (Opus review) | CP-R |
| T-236 | Listen as an audio-output pipeline, per T-221's plan, on this object | T-221, T-230, T-234 | Opus | CP-A |
| T-237 | MUI: competing-pipeline list, evidence ladder, promote/reject, duplicate-group collapse, artifact badge | T-234 | Sonnet | MUI-X |
| T-238 | Blind acceptance: one station → one emitter with N ranked pipelines; the adjacent-station guard case; artifact attribution; promotion confirms | T-233, T-235 | Opus | CP-E |

**Waves** (≤ 4 Rust builders): (1) T-230; (2) T-231, T-232, T-234; (3) T-233, T-235, T-236; (4) T-237, T-238. No Fable tasks.

### 11.10 Open questions (for the user)

1. **Lazy or eager rows.** Materialise a bare `energy` pipeline only on demand (proposed), or give every emitter one from creation so the inventory is uniform, at the cost of a row per box?
2. **Two promoted pipelines.** Is "one promoted decode plus one promoted audio pipeline per emitter" right, or should exactly one pipeline ever be promoted?
3. **Artifact visibility.** Should image/harmonic/intermod-attributed candidates be hidden by default (proposed), or shown greyed under their source so you can see the front end misbehaving?
4. **Supersession margin.** Is 4 bits (16:1) the right bar for one hypothesis to hide another, or should competitors always stay visible until promotion?
5. **User versus decode.** A user-promoted pipeline versus a CRC-valid decode from a different pipeline — user wins (proposed, matching ADR-0016 rank 0), or the decode wins and the contradiction is flagged? Same call as ADR-0016 open question 3.

*Unverified in this amendment: the 0.6 channel-overlap fraction, the 4-bit supersession margin, the SNR × duty × trust proxy for T-219, and the artifact-detection tolerances — all first guesses, to be measured against the real FM capture and the adjacent-station guard case.*
