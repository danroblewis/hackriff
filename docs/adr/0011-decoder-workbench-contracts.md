# ADR-0011 — Decoder workbench contracts: blocks, recipes, field maps, inspector stream

**Status:** PROVISIONAL (T-085, core interface, reviewed before merge)
**Touches:** C20 demodulation, C21 bit-framing, C22 decoders, C24 stream output, C25 recordings; Demodulation / Decode / Bitstream ([docs/07 §2.14–2.16](../07-data-model.md)); [ADR-0001](0001-pipeline-runtime.md), [ADR-0003](0003-process-plugin-model.md), [ADR-0004](0004-stream-output-contract.md); brief [docs/13](../13-m1-decoder-workbench.md)
**Code:** `crates/hk-recipe` (recipe, parameter and field-map schemas), `crates/hk-blocks` (block contract, buffers, status, registry, catalogue), `crates/hk-stream/src/inspector.rs` (inspector wire types), `recipes/rds.recipe.json` (worked example). Wire spec: [`docs/stream-contract.md` §14](../stream-contract.md). Planned routes: [`docs/api.md` "Decoder workbench"](../api.md).

## Context

M1 re-scopes decoding (docs/13): no Rust module per protocol. Decoders are built **inside** hackriff from reusable blocks and **declarative recipes**, discovered from the signal. External decoders stay as a long-tail escape hatch and as test oracles.

Four contracts gate the rest of M1, and every follow-up task (T-086…T-097) codes against them:
1. the **block contract** (typed ports, parameters, status, stage outputs, lifecycle, real-time rules);
2. the **recipe schema** (a decoder as data, hot-edited without stopping capture);
3. the **field-map schema** (the declarative parser);
4. the **inspector stream** (one byte record per frame over the stream contract).

Constraints carried in:
- ADR-0001: pipelines change without stopping capture or rebuilding; chains are ring readers with a runtime-built node list (S1 owned dataflow).
- ADR-0004 / stream contract: drop, never block; gating at egress; one contract for UI and external programs.
- CLAUDE.md: blind-first (hints never tune), tune from the processed output, UI is a thin client, receive-only, legal gating of restricted content.

## Decision summary

| Question | Decision |
|---|---|
| Where the contracts live | New light crate **`hk-recipe`** (schemas, no DSP) + new crate **`hk-blocks`** (block trait, buffers, registry, implementations). Inspector wire types in **`hk-stream::inspector`** next to the audio and burst profiles. |
| Port types | Closed set: `iq` (Complex32), `real` (f32), `soft` (f32, positive = 1), `bits` (u8 0/1), `frames` (byte records + layer tree). |
| Recipe format | **JSON** (serde types are the schema of record; unknown fields are errors). |
| Graph shape | Ordered node list = linear chain by default; explicit `inputs` make a DAG (fan-out, multi-input blocks). Validated acyclic and fully typed. |
| Hot edit | New graph built off the real-time thread, swapped at a chunk boundary; unchanged nodes keep state; hot parameters apply in place; downstream of a cold change resets. Capture and the ring are never touched. |
| Field maps | Nested fields `{name, type, offset, length, unit bits|bytes, endianness, bit_order, condition, repeat}`; types `uint int enum ascii bitfield bytes layer`; structured JSON conditions. |
| Linked selection | Evaluator emits absolute `bits` and `bytes` ranges per node plus a per-byte leaf index; the UI does no range arithmetic. |
| Inspector framing | A `messages` stream of **`frame` records** (NDJSON), gated by the §6 message rules: metadata (frame, sample index, channel, bit length, revision, fit) always flows; bytes and layers are content. |
| Recorded decoded streams | Stored as the §3 byte stream itself (header + frame records without layers); re-parse = read, evaluate, re-emit. |
| follow_hops | Recipe-level `input.channels: follow-hops` + one `follow_hops` merge node; upstream of it is instantiated per channel. |

## 1. Block contract

### 1.1 Ports and sample types

`hk_recipe::PortType`, closed. Adding a type is a contract change.

| Type | Element | Rate | Real-time class |
|---|---|---|---|
| `iq` | `Complex32` | sample rate | sample-rate: no steady-state allocation |
| `real` | `f32` (discriminator, MPX, envelope) | sample rate | sample-rate |
| `soft` | `f32` soft symbol/bit, positive = 1 (the §13.3 polarity) | symbol rate | sample-rate |
| `bits` | `u8`, 0 or 1 | bit rate | sample-rate |
| `frames` | packed bytes + `FrameInfo` (index, source sample index, channel, bit length, check status, corrected bits, optional layer tree) | frame rate | frame-rate: bounded allocation per frame allowed |

Buffers (`hk_blocks::buffer`):
- `PortSlice<'a>`: borrowed input items.
- `PortVec`: owned output items, pre-sized at `init`.
- `FrameBuf`: one byte arena plus per-frame info, cleared with capacity kept.

**Frame packing** is fixed: bit 0 is the MSB of byte 0; `bit_len` may be any length, and the last byte is zero-padded. Air bit order is the recipe's business (a block reverses bits if the protocol is LSB-first on air), so the inspector, field maps and stored streams all share one convention.

Each chunk carries a `ChunkMeta`:
- `index`: the element count on this port;
- `source_index` and `source_per_item`: the time map back to the ring's sample counter, C03, so every frame gets a sample index and a timestamp;
- `rate_hz` and `channel`;
- `flags`: `DISCONTINUITY` (start, gap, lost ring samples, retune: drop history), `RESET` (a hot edit rebuilt this or an upstream node), `CHANNEL_CHANGE`, `END` (flush). Flags propagate downstream.

Port polymorphism: an input may accept several types, e.g. `clock_recovery` takes `iq` or `real`. An output listing several types means "same type as the block's input". Recipe validation resolves that in topological order.

### 1.2 Parameters, descriptors, registry

Every block kind publishes a `BlockDescriptor`, served by the planned `GET /api/blocks`:
- `name`, `version`, `group`, `doc`;
- `inputs` and `outputs` (`PortSpec`: name, accepted types, `diagnostic`);
- `params`: a list of `ParamSchema`.

A `ParamSchema` has a `name`, a type and `required`, `default`, `hot` and `doc`. Parameter types:
- `bool`, `int {min,max}`, `float {min,max,unit}`, `enum {values}`, `string {max_len}`;
- `hex {max_bits}`: JSON has no hex literals; polynomials and sync words are written `"0x5B9"`;
- `field-map`: an id in the recipe;
- `field-path`;
- `list {item}` and `object {fields}`.

Validation (`ParamSchema::validate_all`) rejects unknown keys, missing required keys, wrong types and out-of-range values. Error messages never echo values. Cross-parameter rules (e.g. `sync_word` needed when `mode = sync-word`) are the block's own build-time `BlockError::Params`.

`hot: true` means a change is applied in place at the next chunk boundary without losing state (§2.3). Thresholds, loop bandwidths and field maps are hot. Anything that changes a filter design, rate or frame geometry is cold.

`BlockFactory::build(params, BuildCtx)` makes instances off the real-time thread. `Registry` maps names to factories, validates params before building, and implements `hk_recipe::Catalogue` so recipes validate against what is actually implemented.

### 1.3 Status readout and stage outputs

`Block::status() -> Status` is `Copy` and never allocates. It carries:
- `lock` (`none`/`searching`/`locked`);
- `snr_db`, `error_rate` (BER or block/CRC failure ratio, in [0, 1]) and `quality` (in [0, 1]);
- `items_in` and `items_out`;
- up to six block-specific numeric `extra`s.

The runtime polls it about every 250 ms. It serves three uses:
- **status records** on the pipeline's streams, as flat allowlist-shaped metadata (`Status::to_metadata`, §14.3);
- `GET /api/pipelines/{id}`;
- **output-driven refinement**: a recipe's `refine.objective` names a node and a metric (e.g. `crc.error_rate → min`), and the pipeline's refinement loop (`hk_pipeline::refine`, T-070) tunes centre and bandwidth from it.

**Stage outputs:**
- Every output port of every node is a stage output; blocks do nothing to offer one. The runtime taps `(node, port)` after `process` only while a consumer is open, so an untapped stage costs nothing.
- A **diagnostic** output (`PortSpec::diagnostic`, e.g. `clock_recovery.timing_error`) is computed only while tapped (`Io::tapped`).
- Rendering reduction, e.g. the `spectrum` view of an `iq`/`real` port, is server-side, so the thin UI never processes samples.

### 1.4 Lifecycle and real-time rules

```
build(params) → init(input PortInfos) → output PortInfos
             → process(Io) per chunk ─┬─ update_params(params) → Applied | Rebuild   (between chunks)
                                      ├─ reset()                                    (between chunks)
                                      └─ status()                                   (any time between chunks)
```

1. **`init` allocates, `process` doesn't.**
   - `init` negotiates rates, designs filters and returns each output's `max_items` (the most items one chunk can carry) and `hold_items` (history held before output reflects an input, used for the latency bound). The runtime pre-sizes outputs from `max_items`.
   - On `iq`/`real`/`soft`/`bits` ports, `process` must not allocate in steady state. T-088 adds a counting-allocator test in the style of `hk-stream/tests/alloc_free.rs`.
   - On `frames`, per-frame allocation (layer trees, strings) is allowed but bounded (`MAX_LAYER_NODES`, `MAX_FIELDS`).
2. **`process` never blocks, sleeps or does I/O** and consumes the whole input chunk. Output metadata: the runtime calls `Output::begin_chunk` (index advanced, items and flags cleared); the block sets the time map and flags and appends items.
3. **Bounded latency.** Output for an input item appears within `hold_items` further items. Frame-assembling blocks emit a frame as soon as its last bit arrives; `follow_hops` holds at most `order_window_s`. The runtime reports the pipeline's bound and measured latency in chain stats (§12.1 `chain_stats[]`).
4. **Backpressure: drop, never block, consistent with the stream contract.**
   - A recipe pipeline is a ring reader like every runtime chain (ADR-0001 S1). If it falls behind, the ring laps it; the reader counts `lost_samples`, and the next chunk carries `DISCONTINUITY`, so blocks re-acquire rather than stall capture.
   - Egress is §7 unchanged: per-consumer bounded queues with drop markers.
   - Stage taps and inspector streams are publishers like any other. Nothing in the block contract can apply backpressure to the ring.
5. **Admission.** Recipe pipelines are a new chain kind (`recipe`) in the per-run chain budget (§12.1 T-071), costed from `input.sample_rate_hz` and measured thread CPU. Beyond budget a start answers `503 busy`, and running chains are untouched.
6. **Errors.** A `process` error stops that pipeline (not the run), is reported in its status, and leaves the ring and other chains alone.

### 1.5 The M1 catalogue

`hk_blocks::catalogue::planned()` pins names, groups and port signatures for every M1 block in docs/13. Parameter schemas are pinned for the blocks the RDS reference uses; the others have placeholder schemas that their implementing task pins. A placeholder's params are accepted with a warning.

A test (`implemented_blocks_match_their_pinned_descriptors`) fails if an implementation's ports or pinned params drift from this catalogue. Changing a pinned descriptor is a contract change reviewed like this ADR.

| Group | Block | Ports | Params |
|---|---|---|---|
| iq | `mix` | iq → iq | placeholder |
| iq | `lowpass`, `resample` | iq\|real → same | placeholder |
| iq | `fm_demod` | iq → real | pinned: `deviation_hz` (hot), `output_rate_hz`, `deemphasis_s` |
| iq | `am_demod`, `fsk_demod`, `msk_demod` | iq → real | placeholder |
| iq | `ppm_demod` | iq → soft | placeholder |
| iq | `subcarrier` | real → iq | pinned: `carrier_hz`, `bandwidth_hz`, `output_rate_hz`, `reference {pilot_hz, multiple, pll_bandwidth_hz}`, `phase_tracking` (hot) |
| symbol | `clock_recovery` | iq\|real → soft (+ diagnostic `timing_error` real) | pinned: `symbol_rate_bd`, `pulse`, `algorithm`, `soft_from`, `loop_bandwidth` (hot), `max_deviation_ppm` |
| symbol | `slicer` | soft → bits | pinned: `threshold` (hot), `invert` (hot) |
| symbol | `diff_decode` | bits → bits | pinned: `mode` (hot) |
| symbol | `nrzi` | bits → bits | placeholder |
| symbol | `manchester` | soft\|bits → bits | placeholder |
| framing | `sync_search` | bits → frames | pinned: `mode` (`sync-word`/`offset-words`) + per-mode keys |
| framing | `deframe` | bits\|frames → frames | placeholder |
| framing | `interleave`, `deinterleave` | frames → frames | placeholder |
| fec | `crc` | frames → frames | pinned: RevEng model (`width, poly, init, refin, refout, xorout`), `span` or `blocks {data_bits, check_bits, offsets}`, `strip`, `drop_invalid` (hot), `correct_burst_bits` |
| fec | `bch`, `parity`, `checksum` | frames → frames | placeholder |
| parse | `fields` | frames → frames | pinned: `map` (hot) |
| parse | `text` | frames → frames | pinned: `name`, `key`, `address[]`, `chars[]`, `segments`, `chars_per_segment`, `reset_on`, `terminator`, `emit` (hot), `charset` |
| multi | `follow_hops` | frames → frames | pinned: `dedupe_s` (hot), `order_window_s` |
| util | `identity` | any → same | none (implemented: the contract example) |

### 1.6 Wrapping existing code

Blocks are **adapters over the existing kernels**, not rewrites:
- **mix/lowpass/resample:** hk-dsp `filter::Nco`, `design_lowpass` + `kernels`, and the DDC's polyphase stage. hk-dsp's `Ddc` wants a `ProvenanceHandle`, which `ChunkMeta` doesn't carry, so T-086 may expose a provenance-free resampler additively.
- **fm_demod/subcarrier/clock_recovery:** the hk-demod discriminator (`dsp`), `pilot::PilotPll` and `rds::demod`'s biphase timing.
- **fsk_demod/slicer:** hk-demod `fsk`.
- **sync_search:** hk-estimate `framing::sync` and hk-demod `rds::block` syndromes. **crc:** hk-estimate `framing::crc::CrcCore`.

Rules:
- The recipe runtime's **input stage** is the runtime's own DDC over ring chunks, which do carry provenance. Blocks after it see only `ChunkMeta`.
- Pull-style kernels that return a fresh `Vec` per call (`RdsDemod::take_bits`, `RdsDecoder::take_groups`) get additive `drain_into(&mut Vec)` variants before they sit on a sample-rate port. They are acceptable as-is on frame-rate outputs.
- Existing kernels keep their own tests. A block adds a contract test (chunking invariance: output is identical however the input is split) and a synthetic-signal test.
- The M0 chain shapes (`analog-auto`, `fsk-bursts`, `plugin`, `hk_pipeline::chains::spec`) are unchanged. Recipes are a fourth chain kind beside them, and the existing RDS decoder stays as the oracle for Tutorial 1.

## 2. Recipe schema

### 2.1 Format: JSON

JSON, not TOML:
- **One format end to end.** The same document is the file in `recipes/`, the `POST /api/recipes` body, the hot-edit body and what the UI edits. The UI and Python speak JSON natively; TOML would need converting at every API boundary.
- **Repository precedent.** Plugin manifests (§9.1) and chain specs (`ScanPlan.extra.pipeline.chains`) are JSON with unknown fields as errors.
- **No new dependency.** serde_json is already in every crate involved; `toml` would be new in core crates.
- **Deep nesting.** TOML's advantage (comments, hand-editing flat config) is weak for deep arrays of tables such as field-map layers and per-block offset lists, which TOML renders awkwardly. Documentation lives in `description` and `label`.

**Schema of record:** the serde types in `hk_recipe` (`#[serde(deny_unknown_fields)]`), plus `Recipe::validate_structure()` (no catalogue) and `Recipe::validate(&dyn Catalogue)` (full). A generated JSON Schema was not adopted: it would need a new dependency (`schemars`) and could drift from the validation rules, which go beyond what JSON Schema expresses (DAG acyclicity, port typing, field references). The UI gets the schema it needs from `GET /api/blocks` (descriptors) and `POST /api/recipes/validate` (errors with paths).

### 2.2 Document shape

```jsonc
{
  "schema": "hackriff.recipe", "schema_version": 1,
  "id": "rds", "version": 1, "name": "RDS: data on FM", "description": "…",
  "match": { "families": ["wfm"], "freq_hz": [[65.8e6, 108e6]], "bandwidth_hz": [1e5, 3e5],
             "symbol_rate_bd": null, "bursty": false, "features": ["pilot-19k"] },
  "input": { "port": "iq", "sample_rate_hz": 240000, "bandwidth_hz": 200000,
             "channels": { "mode": "single" } },
  "nodes": [
    { "id": "fm", "block": "fm_demod", "params": { "deviation_hz": 75000 } },
    { "id": "rt", "block": "text", "inputs": { "in": "group" }, "params": { "…": "…" } }
  ],
  "field_maps": { "rds_group": { "unit": "bits", "fields": [ "…" ] } },
  "outputs": [
    { "id": "groups", "kind": "inspector", "from": "group" },
    { "id": "station", "kind": "messages", "from": "ps",
      "decode": { "frame_model": "rds-ps", "identity": { "scheme": "rds-pi", "field": "ps.key", "format": "hex" },
                  "metadata": [], "content": ["ps.text"] } },
    { "id": "mpx", "kind": "stage", "from": "fm", "view": "spectrum" }
  ],
  "output_policy": { "content_class": "unrestricted" },
  "refine": { "objective": { "node": "crc", "metric": "error_rate", "goal": "min" }, "tune": ["center_hz"] }
}
```

**Wiring:**
- A node without `inputs` reads the previous node's single output; the first node reads `input`.
- `inputs` maps input port → `input` | `node` | `node.port`.
- Validation checks that ids are unique, references resolve, the graph is acyclic, every input is connected, and port types match (with polymorphic outputs resolved). It also checks that `inspector`/`messages` outputs read `frames` and that `spectrum` views read `iq`/`real`.

**Outputs:**
- `inspector`: frame records, §14.
- `messages`: Decode messages (§5.1), also stored as Decode rows. The `decode` mapping names the frame model, an identity field (feeds the Emitter identity, docs/07 §2.11), metadata field paths and content field paths.
- `stage`: a named default stage stream. Any port can still be tapped ad hoc.

**Input:** `iq` means the runtime down-converts the target (emitter, selection, band, refined channel) to `sample_rate_hz`. `bits`/`soft`/`frames` means the recipe tail runs over a recorded decoded stream (§14.7): the parser-authoring loop without RF.

### 2.3 Hot edit without stopping capture (ADR-0001)

A running pipeline = one ring reader + its channel DDC + an instantiated node graph at recipe revision `(version, edit_rev)`. An edit is a whole new recipe document (the draft); `hk_recipe::EditPlan::between(old, new, hot)` classifies each node:

| Change | Effect at the swap |
|---|---|
| Unchanged | Instance moves over with its state. |
| Params only, all changed keys `hot` | `update_params` on the instance at the chunk boundary; state kept. |
| Params only, any cold key | Rebuilt: fresh instance, `init`. |
| Block, version or inputs changed; node added | Rebuilt. |
| Node removed | Dropped. |
| Surviving node downstream of a rebuilt/added/cold node | `reset()` and a `RESET` chunk flag. |
| Field map changed | Nodes using it swap the map at the next frame (hot by definition); no DSP state touched. |
| `input` changed | Channel re-plumbed, every node rebuilt (a retune-like edit, still no capture stop). |
| Outputs changed | Removed outputs' streams finish; new ones are offered. Unchanged outputs keep their consumers and `seq`. |

**Mechanics** (T-088):
1. **Validate the draft.** An invalid draft is refused (`400` with paths), and the running revision is untouched.
2. **Build off the real-time thread.** New and rebuilt instances are made with `build` + `init` on a control thread, while the old graph keeps processing.
3. **Swap.** At the first chunk boundary after the build completes, the pipeline thread exchanges graphs in O(nodes). The ring reader's cursor is untouched, so **no sample is lost** and capture never pauses.
4. **Report.** The result names `applied_at_sample`, the new `edit_rev` and the plan. An `edit` record (§14.3) marks the boundary on the pipeline's streams, and every later frame carries the new `edit_rev`.

Hot edits don't save. `POST /api/pipelines/{id}/save` writes the running revision as the recipe's next version.

### 2.4 Versioning, save, list, re-run, matching

- `schema_version` is the format version (1). An unknown major is refused; new optional keys need a new schema version, because unknown fields are errors.
- `version` is per recipe id, from 1, monotonic, and **immutable once saved**. Saving always creates `latest + 1`, so old versions stay re-runnable and a stored decoded stream names the exact version (and `edit_rev`) that produced it.
- A node may pin a block `version`; a mismatch is a validation error. A block's descriptor version bumps when a change would invalidate or alter existing recipes.
- **Storage:**
  - Built-in recipes: `recipes/*.recipe.json` in the repository (read-only; a user save of a built-in id writes into the data directory).
  - User versions: `<data dir>/recipes/<id>/<version>.json`.
  - Listing merges both.
- **Re-run:** `POST /api/pipelines {recipe_id, version?, target}`, where target is an emitter, selection, band or recorded capture. Several pipelines may run the same recipe on different targets.
- **Matching is a hint, never a tune.** `GET /api/recipes/match?emitter=<id>` ranks recipes by how well the emitter's measured family, bandwidth, symbol rate, burstiness and features fit `match`, with reasons. `freq_hz` only raises rank for an emitter already found there by blind detection. Nothing tunes to it or auto-starts a recipe. The user or an explicit scan-plan rule starts pipelines.

### 2.5 Parallel pipelines and `follow_hops`

- **Parallel pipelines** are independent ring readers (one thread each), sharing only the ring and the chain budget. Each has its own id, streams (`inspector/<pipeline>/<output>`), counters and edit history. Two pipelines on one emitter (e.g. RDS plus a trial parser) are allowed.
- **`follow_hops`** (one pipeline spanning several channels):
  - The recipe declares `input.channels = {mode: "follow-hops", channel_bandwidth_hz, max_channels, band_hz?, list_hz?}` and contains exactly one `follow_hops` node, whose input is `frames`.
  - The runtime instantiates **every node upstream of `follow_hops`** once per channel (per-channel state, its own DDC). Each instance stamps `ChunkMeta::channel`/`FrameInfo::channel`.
  - All instances feed the single `follow_hops` node, which merges frames in source-time order within `order_window_s` and drops duplicates within `dedupe_s`. Everything downstream runs once.
  - Channels come from blind detections in the band (default) or `list_hz`; `max_channels` bounds them.
  - Every frame record carries `channel` and `channel_hz`, and the header lists channels known at open.
  - Cross-channel **fusion** (decoding one stream to steer another, e.g. trunking) is M4, not this.

### 2.6 Content class

`output_policy` has the same shape and rules as a plugin manifest's `output` (§9.3):
- `content_class` is a **ceiling**. The effective class of everything a pipeline emits is `clamp(ceiling, source class)`, where the source class comes from `hk_pipeline::class` for the target. A recipe can restrict itself, never upgrade.
- A class that forbids content **must** declare `metadata_keys` (possibly empty); validation refuses it otherwise.
- POCSAG (Tutorial 2) therefore declares `restricted-paging` with the existing paging allowlist (capcode, function, baud, encoding). Its inspector frames go out metadata-only unless a user classification rule vouches for the emitter (own pager). This is the existing rule, not a new one.
- Recorded decoded streams follow the same rule (§14.7).

## 3. Field maps (declarative parser)

### 3.1 Schema

`hk_recipe::FieldMap`: `{description, unit (default bytes), endianness (default big), bit_order (default msb), fields[]}`. Each `Field`:

| Key | Meaning |
|---|---|
| `name` | `[a-z][a-z0-9_]*`, unique among siblings |
| `type` | `uint`, `int` (two's complement), `enum` (+ `values: {"0": "A"}`), `ascii` (+ `charset` ascii/latin1/rds, `char_bits` 8/7), `bitfield` (+ `flags: [{name, bit}]`, bit 0 = first bit on air), `bytes` (uninterpreted), `layer` (+ `fields`) |
| `offset` | From the enclosing layer's start, in `unit`; omitted = after the previous *present* sibling |
| `length` | A number of `unit`s; `"remainder"`; or `{field, scale, add}` (a length field). Integers are ≤ 64 bits and need a fixed length. |
| `unit` | `bits` or `bytes` (override) |
| `endianness` | `big` or `little`: byte order of whole-byte integers |
| `bit_order` | `msb` or `lsb`: sub-byte fields and characters (POCSAG 7-bit LSB-first text) |
| `condition` | `{field, eq\|ne\|lt\|le\|gt\|ge\|in}` combinable with `{all:[…]}`, `{any:[…]}` and `{not:…}`. False = absent, not an error. |
| `repeat` | A count (fixed, `{field,…}` or `"remainder"`); instances addressed `name[i]` |
| `display`, `label` | Rendering (`dec`, `hex`, `bin`, `bool`) and a human label |

**References** (condition, length and repeat) name an **earlier**, non-repeated integer field. A bare name resolves to the nearest earlier sibling, then up through the ancestors; a dotted path resolves from the root.

**Conditions are structured JSON, not an expression string.** No parser or grammar is needed, the UI builds them from widgets, and they are trivially validated.

**Bounds:** depth ≤ 16, ≤ 4096 fields, repeat ≤ 65 536, ≤ 4096 layer-tree nodes per frame.

### 3.2 Ranges and linked selection

The evaluator (T-089) produces `hk_stream::inspector::LayerTree`:
- `nodes[]` in pre-order, each with `id`, `parent`, `name`, `path`, `type`, `bits: [offset, length]` (absolute from the frame's first bit), `bytes: [first, end)` (`byte_span`), `value`, `text`, `label` and `error`. Bitfield flags are child `flag` nodes, so a single bit is selectable.
- `byte_index[b]`: the ids of the **leaf** nodes overlapping byte `b`, in bit order.

**Linked selection** uses only these: click a field → highlight `bytes` (and `bits` for sub-byte precision); click a byte → select `byte_index[b][0]`, with repeat clicks cycling. The server computes the mapping (`LayerTree::index_bytes`), so the UI does no range arithmetic.

Values:
- integers ≤ 2⁵³ are JSON numbers, larger ones decimal strings;
- `ascii` is a string, a flag a bool;
- `text` is always the rendered form.

### 3.3 Errors

- **Static** (`FieldMap::validate`, at save/validate time; the map is wrong for every frame):
  - bad or duplicate names;
  - type-specific keys on the wrong type;
  - integer width > 64 or not fixed;
  - ascii length not a whole number of characters;
  - enum keys not integers;
  - flags outside the field;
  - a fixed-size field ending past a fixed-size layer;
  - references to unknown, later, repeated or non-integer fields;
  - conditions with other than one operator;
  - depth or size limits exceeded.

  Each error carries a dotted path.
- **Fit** (per frame, at evaluation; the map is valid but this frame doesn't fit): `out-of-bounds` (with `need_bits`/`have_bits`), `bad-length`, `missing-reference` (the referenced field is absent in this frame), `repeat-limit`, `node-limit`.
  - Evaluation **never aborts the frame**: the failing node is marked `error`, its later siblings are still tried, and the frame's `fit` is `ok`/`partial`/`failed`.
  - Over a recording, `POST /api/captures/{id}/parse` returns a `fit` summary (frames ok/partial/failed, errors by path). That is the "does my guess hold across many frames" signal of the authoring loop.

## 4. Inspector stream

Specified in [`docs/stream-contract.md` §14](../stream-contract.md) (1.2 draft); types in `hk_stream::inspector`. In brief:
- **Stream:** a `messages` stream with `message_schema: "hackriff.inspector/1"` and an optional header `inspector` object `{pipeline_id, recipe_id, recipe_version, output_id, source (live | capture), channels[]}`. Stream id `inspector/<pipeline>/<output>`.
- **One `frame` record per frame:**
  - `seq`, `t` (time of the first bit), `content_class`, `gated`, `crc_status`, `decoder` (`recipe:<id>@<version>`), `frame_model`, `emitter_id`;
  - `metadata {frame, sample_index, channel, channel_hz, bit_len, recipe_version, edit_rev, fec_corrected_bits, fit}`;
  - `content {hex, layers}`.
- **Interleaved records:** `status` (per node, ~4 Hz) and `edit` (hot-edit boundary).
- **Why messages, not a new binary kind:**
  - A new `kind` value is a major change under §1; a new message record type is a minor one, and pre-1.2 readers skip it.
  - The §6 message gate (metadata always flows, content gated, allowlist under restricted classes) is exactly the needed legal behaviour, already tested.
  - Frames are small and slow (RDS ~11 groups/s, ADS-B ≲ 2000 frames/s at ≤ 14 bytes), so JSON cost is irrelevant.
  - Browsers and Python read it without a binary parser.
- **Stage streams** (§14.4) use the existing binary kinds: `iq`, `audio` (rf32), `symbols`, `bits`, `spectrum`.
- **Recorded decoded streams** (T-092) are files holding the §3 byte stream itself: the header, then frame records **without** `layers` (layers are derived). Re-parsing reads the records, evaluates a (new) field map and re-emits frames with layers and `source: {kind: capture, reparse: true}`. Frames whose class forbids content are stored metadata-only, so they cannot be re-parsed, by design.

## 5. Worked example: RDS

`recipes/rds.recipe.json` is Tutorial 1 expressed in the schema. Every block, parameter and port type validates against the pinned catalogue (`crates/hk-blocks/tests/rds_recipe.rs`).

**Chain:**
1. `fm` `fm_demod` (iq 240 kS/s → real MPX);
2. `rds57` `subcarrier` (57 kHz = 3 × 19 kHz pilot reference, 4.8 kHz wide, 9.5 kS/s, BPSK phase tracking; real → iq);
3. `clock` `clock_recovery` (1187.5 Bd, biphase pulse, max-contrast timing; iq → soft);
4. `slice` `slicer` (→ bits);
5. `diff` `diff_decode` (xor);
6. `sync` `sync_search` (`offset-words`: 26-bit blocks, 10 check bits, poly 0x5B9 (full form: `poly` accepts the RevEng normal form or the full form, and the x^width term is implied either way), offsets A 0x0FC, B 0x198, C 0x168, C′ 0x350, D 0x1B4, sequence A, B, C|C′, D → 104-bit group frames);
7. `crc` `crc` (block mode, per-position offsets, strip → 64-bit frames with `crc_status`);
8. `group` `fields` (map `rds_group`);
9. `ps` `text` (PS: key PI, address `ps.segment`, 4 × 2 chars);
10. `rt` `text` (RadioText: address `radiotext.segment`, chars `chars_a` for 2A or `chars_b` for 2B, 16 segments, reset on the A/B flag, terminator 0x0D).

**Field map `rds_group`** (bits):
- `pi` 0–16 (hex), `group_type` 16–20, `version` 20 (enum A/B), `tp` 21 (bool), `pty` 22–27 (numeric: RDS and RBDS name tables differ);
- layer `ps` 27–64 when `group_type == 0`: bitfield `flags` {ta, music, di}, `segment` (2), `block3` (16, AF or PI), `chars` (16, RDS charset);
- layer `radiotext` 27–64 when `group_type == 2`: `ab`, `segment` (4), then `chars_a` (32) if `version == 0`, else `pi_repeat` (16) + `chars_b` (16).

**Outputs:**
- `groups` (inspector);
- `group-info` (messages: identity `rds-pi` from `pi`; metadata group_type, version, tp, pty);
- `station` (messages: PS text, identity from `ps.key`);
- `radiotext` (messages);
- stage `mpx` (spectrum view) and `symbols` (the eye/soft symbols).

**Policy and refinement:** `unrestricted` (broadcast RDS is public station identity, as in hk-demod); `refine` minimises `crc.error_rate` over `center_hz`.

**Oracle tie-in.** The test asserts the recipe's polynomial, offset words (and their block positions) and bit rate equal `hk_demod::rds` constants. T-094 then asserts the recipe's decoded PI/PS/PTY/RT equal the existing decoder's on `fm_100p8M` through the mock SDR.

## 6. Crate placement

| Crate | Holds | Depends on | Why |
|---|---|---|---|
| `hk-recipe` (new) | `Recipe`, `NodeSpec`, `OutputSpec`, `PortType`, `ParamSchema`/`ParamType`, `BlockDescriptor`, `Catalogue`, `FieldMap`, `EditPlan`, validation | hk-model, hk-stream, serde, serde_json | Pure data, no DSP. hk-api, hk-store (captures) and hk-estimate (authoring assist emits field-map/param suggestions) use it without linking the block library. |
| `hk-blocks` (new) | `Block`, `Io`, `PortInfo`, buffers, `Status`, `BlockFactory`, `Registry`, catalogue, all block implementations | hk-recipe, hk-dsp, hk-estimate, hk-demod, hk-stream, hk-model | Blocks wrap kernels from three DSP crates. Putting them in hk-dsp would invert dependencies (hk-dsp can't see hk-demod); putting them in hk-pipeline would force every block test to compile the whole composition. The DSP dependencies are pre-added so T-086/T-087 never touch the manifest. |
| `hk-stream` (`inspector` module) | Frame/status/edit record types, `LayerTree`, `byte_span`, hex helpers | (existing) | The wire contract and its gate live here, like the `audio` and `bursts` profiles. |
| `hk-pipeline` (T-088) | Recipe runtime: graph build/swap, runner, channel DDC, taps, publishers, recipe store, chain-budget kind | + hk-blocks, hk-recipe | Composition belongs here (ADR-0001 S1 chains). |
| `hk-api` (T-088/T-089/T-091/T-092) | Routes over the runtime and store | + hk-recipe | Thin HTTP over the above. |

## 7. Parallel-work map

File ownership so T-086…T-093 run in separate worktrees without collisions. "Shared" rows have one owner; other tasks only read them or append a clearly separated block.

| Task | Owns (writes) | Reads / codes against | Notes |
|---|---|---|---|
| **T-086 Blocks A** | `crates/hk-blocks/src/blocks/iq/**`, `crates/hk-blocks/src/blocks/symbol/**`; additive `pub` helpers in hk-dsp (`filter`, `ddc` resampler) and hk-demod (`dsp`, `pilot`, `rds::demod` `drain_into`) | `block.rs`, `buffer.rs`, `status.rs`, catalogue test | Pins placeholder params in its own `planned()`. **Coordinate with T-084** in hk-demod `rds` (T-084 fixes RDS HIL findings): additive new functions only, no edits to T-084's lines. |
| **T-087 Blocks B** | `crates/hk-blocks/src/blocks/framing/**`, `crates/hk-blocks/src/blocks/fec/**`; additive `pub` in hk-estimate `framing::{crc,sync}` | as above; hk-demod `rds::block` (read-only) | Unit tests: RDS offset words + 0x5B9, POCSAG 0x7CD215D8 + BCH(31,21), CRC-16, CRC-24. |
| **T-088 Recipe runtime** | `crates/hk-pipeline/src/recipes/**` (new: `runtime.rs`, `graph.rs`, `swap.rs`, `taps.rs`, `store.rs`, `openers.rs`); one `recipe` kind in `chains/budget.rs`; `crates/hk-api/src/recipes.rs` (new); its rows in `http.rs::ROUTES`; `docs/api.md` "Recipes and pipelines" subsection; `crates/hk-recipe/src/edit.rs` extensions | hk-recipe, hk-blocks contract, `hk_stream::inspector` types | Publishes frames through a `FrameSink` trait in `recipes/` until T-089's `Publisher::publish_frame` merges, then swaps to it (one-line change). Allocation-free runner test. |
| **T-089 Parser + inspector API** | `crates/hk-recipe/src/fields/eval.rs` (new evaluator; `fields.rs` becomes `fields/mod.rs` with the schema unchanged); `crates/hk-blocks/src/blocks/parse/**`; `crates/hk-stream/src/inspector.rs`, and `publish_frame`/`publish_record` plus the optional `inspector` header field in `publisher.rs`/`header.rs` (reviewed: legal gate); bump `STREAM_VERSION_MINOR` to 2; `crates/hk-api/src/inspector.rs` (new: `open/inspector`, `POST /api/captures/{id}/parse`); its `ROUTES` rows; `docs/api.md` "Inspector" subsection; `docs/stream-contract.md` §14 finalisation | hk-recipe schema | Contract tests in `crates/hk-cli/tests/api_contract.rs` (append a separate test fn). |
| **T-090 Inspector UI** | `ui/src/inspector/**` | `docs/api.md`, §14 | Thin client: renders `nodes`, `bytes`, `byte_index` verbatim. |
| **T-091 Authoring assist** | `crates/hk-estimate/src/assist/**` (new; reuses `framing::search`); `crates/hk-api/src/assist.rs` (new); its `ROUTES` rows and `docs/api.md` "Assist" subsection | hk-recipe (emits `FieldMap` fragments and `sync_search`/`crc` param objects as suggestions with scores) | Adds `hk-recipe` to hk-estimate's dependencies (no cycle: hk-recipe depends on no DSP crate). |
| **T-092 Decoded capture + scrub** | `crates/hk-store/src/decoded.rs` (new: §3-stream capture writer/reader, quota); `crates/hk-pipeline/src/recipes/capture.rs` (new file inside T-088's directory, after T-088 merges); `crates/hk-api/src/captures.rs` (new, list/frames routes); `ui/` scrub hooks | §14.7, `hk_stream::inspector`, `StreamReader` | Depends on T-088. |
| **T-093 follow_hops** | `crates/hk-blocks/src/blocks/multi/**`; `crates/hk-pipeline/src/recipes/hops.rs` (new, after T-088); hk-core only if a multi-channel reader primitive is needed | §2.5 | Depends on T-088. |
| **T-094…T-097 Tutorials** | `recipes/<name>.recipe.json`, `tests/e2e/tests/acceptance/m1_<name>.rs`, `docs/tutorials/` | everything | T-094 edits `recipes/rds.recipe.json` only to tune params and bumps `version` for any semantic change. |

**Shared touch points and their rule:**
- `crates/hk-api/src/http.rs::ROUTES` and `docs/api.md`: each task appends its own contiguous block and subsection. Conflicts are textual and trivial; merge in task order.
- Workspace `Cargo.toml`: no task needs to edit it (both new crates are already members).
- `crates/hk-blocks/Cargo.toml`: complete, don't edit.
- `crates/hk-blocks/src/blocks/mod.rs`: final (`register_all` already calls every group).

## Options considered

- **Block placement in hk-dsp / hk-pipeline.** Rejected (§6): dependency inversion, or compile coupling.
- **A single "workbench" crate for schemas and blocks.** Rejected: the API, store and assist would link all DSP to read a JSON document.
- **TOML recipes.** Rejected (§2.1). **YAML.** Rejected: needs a new dependency, and indentation-sensitive edits over an API are error-prone.
- **String expression language for conditions.** Rejected for M1 (§3.1). A later `expr` key is an additive schema-version change if needed.
- **Binary inspector records** (new stream kind `frames`). Rejected: a major contract change, re-implementing gating for a new kind, and no throughput need (§4).
- **Recipes as a GNU Radio–style flowgraph with arbitrary Python blocks.** Rejected: the real-time path is Rust (CLAUDE.md), and runtime code loading is what ADR-0001 designs out. Blocks are compiled once; recipes are data.
- **Per-node stream tags (GNU Radio tags).** Deferred: `ChunkMeta` flags plus per-frame `FrameInfo` cover M1's needs (discontinuities, channel, time map, check status). Tags can be added as an optional side channel later.

## Consequences

- New decoders are JSON documents validated against a typed catalogue; nothing is recompiled to build, edit or run one, and capture never stops (ADR-0001's hard requirement, for decoding).
- The inspector, external programs and stored captures share one frame format and one gate.
- M1 fans out into eight worktrees with disjoint file ownership (§7).
- Cost: a second chain model beside the M0 chain shapes until those are re-expressed as recipes (not planned for M1). The pinned catalogue must be kept in step with implementations (enforced by test).

## Open questions (for review)

1. **PTY names.** The RDS worked example keeps PTY numeric because RDS and RBDS tables differ. Should recipes gain a region-conditional enum (`values_by_region`), or should the text block look names up?
2. **Recipe-level JSON Schema for editors.** If the UI wants inline validation before `POST /api/recipes/validate`, generate one from `GET /api/blocks` at runtime rather than maintaining a static file.
3. **Status cadence.** 250 ms per node; a 10-node pipeline is 40 status records/s. Batch all nodes into one record per tick (the likely T-088 choice)?
