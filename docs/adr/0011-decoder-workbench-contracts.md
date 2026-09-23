# ADR-0011 — Decoder workbench contracts: blocks, recipes, field maps, inspector stream

**Status:** PROVISIONAL (T-085, core interface, reviewed before merge)
**Touches:** C20 demodulation, C21 bit-framing, C22 decoders, C24 stream output, C25 recordings; Demodulation / Decode / Bitstream ([docs/07 §2.14–2.16](../07-data-model.md)); [ADR-0001](0001-pipeline-runtime.md), [ADR-0003](0003-process-plugin-model.md), [ADR-0004](0004-stream-output-contract.md); brief [docs/13](../13-m1-decoder-workbench.md)
**Code:** `crates/hk-recipe` (recipe, parameter and field-map schemas), `crates/hk-blocks` (block contract, buffers, status, registry, catalogue), `crates/hk-stream/src/inspector.rs` (inspector wire types), `recipes/rds.recipe.json` (worked example), `recipes/{pocsag,acars,adsb}.recipe.json` (skeletons, §5.1). Wire spec: [`docs/stream-contract.md` §14](../stream-contract.md). Planned routes: [`docs/api.md` "Decoder workbench"](../api.md).

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
| Port types | Closed set: `iq` (Complex32), `real` (f32), `soft` (f32, positive = 1), `bits` (u8 0/1), `frames` (byte records + shared layer tree). |
| Frame length | Fixed, or variable: `length_from` (a field decides it, e.g. the ADS-B DF → 56/112) and `terminator` (a closing word, e.g. ACARS ETX, plus trailer bits), with `frame_bits` as the maximum; shared by `sync_search` and `ppm_demod` (§1.5). |
| Recipe format | **JSON** (serde types are the schema of record; unknown fields are errors). |
| Graph shape | Ordered node list = linear chain by default; explicit `inputs` make a DAG (fan-out, multi-input blocks). Validated acyclic and fully typed. |
| Hot edit | New graph built off the real-time thread, swapped at a chunk boundary; unchanged nodes keep state; hot parameters (field-map content included) apply in place; downstream of a cold change resets. Capture and the ring are never touched. |
| Field maps | Nested fields `{name, type, offset, length, unit bits|bytes, endianness, bit_order, condition, repeat}`; types `uint int enum ascii bitfield bytes layer`; `ascii` with 8/7/4-bit characters (`pocsag-bcd`) and a parity bit; integers with `skip_bits` and `scale`/`add`/`value_unit`; structured JSON conditions. |
| Linked selection | Evaluator emits absolute `bits` and `bytes` ranges per node plus a per-byte leaf index; the UI does no range arithmetic. |
| Inspector framing | A `messages` stream of **`frame` records** (NDJSON), gated by the §6 message rules: bytes and layers are content; metadata (frame, sample index, channel, bit length, revision, fit) flows, reduced to the recipe's `output_policy` allowlist when the class forbids content (stream contract §14.5). |
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
| `frames` | packed bytes + `FrameInfo` (index, source sample index, channel, bit length, check status, corrected bits, optional layer tree as `Arc<LayerTree>`, so frames → frames pass-through blocks don't clone it) | frame rate | frame-rate: bounded allocation per frame allowed |

Buffers (`hk_blocks::buffer`):
- `PortSlice<'a>`: borrowed input items.
- `PortVec`: owned output items, pre-sized at `init`.
- `FrameBuf`: one byte arena plus per-frame info, cleared with capacity kept.

**Frame packing** is fixed: bit 0 is the MSB of byte 0; `bit_len` may be any length, and the last byte is zero-padded. Air bit order is the recipe's business, so the inspector, field maps and stored streams all share one convention:
- whole LSB-first characters (ACARS) are reversed by the framing block: `sync_search` `bit_order: "lsb"` reverses every 8 bits of the frame body before packing (the sync word stays in air order);
- LSB-first sub-byte or cross-codeword characters (POCSAG 4-bit BCD and 7-bit text) stay in air order in the frame and are read with the field map's `bit_order: "lsb"`.

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
- **status records** on the pipeline's streams: **one record per tick for the whole pipeline**, every node batched as flat allowlist-shaped `<node>.<metric>` keys (`Status::to_metadata(node, &mut map)`, §14.3);
- `GET /api/pipelines/{id}`;
- **output-driven refinement**: a recipe's `refine.objective` names a node and a metric (e.g. `crc.error_rate → min`), and the pipeline's refinement loop (`hk_pipeline::refine`, T-070) tunes centre and bandwidth from it.

**Stage outputs:**
- Every output port of every node is a stage output; blocks do nothing to offer one. The runtime taps `(node, port)` after `process` only while a consumer is open, so an untapped stage costs nothing.
- A **diagnostic** output (`PortSpec::diagnostic`, e.g. `clock_recovery.timing_error`) is computed only while tapped (`Io::tapped`). It is a tap for `outputs[]`, never a node input: `Recipe::validate` refuses `node.port` wiring from a diagnostic port (§9.2, T-609).
- Rendering reduction, e.g. the `spectrum` view of an `iq`/`real` port, is server-side, so the thin UI never processes samples.

### 1.4 Lifecycle and real-time rules

```
build(params) → init(input PortInfos) → output PortInfos
             → process(Io) per chunk ─┬─ update_params(params, ctx) → Applied | Rebuild (between chunks)
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

**Frame length** (`hk_blocks::schema::frame_length`, used by every frame-producing block). A frame ends at the first of:
1. `length_from {offset_bits, bits, cases[{min, max, frame_bits}], scale, add, default_bits}`: a field of the frame read MSB first (after `bit_order` is applied); the first case holding its value sets the length, else `value × scale + add` when `scale > 0`, else `default_bits`. ADS-B: DF (bits 0–5) 16–31 → 112, else 56. The block reads the field as soon as its bits arrive.
2. `terminator {words, bits, step_bits, trailer_bits}`: a closing word checked at multiples of `step_bits` from the frame start, then `trailer_bits` more (ACARS: ETX/ETB at character boundaries, then the 16-bit block check).
3. `frame_bits`: the fixed length, or the maximum when (1) or (2) is set. A frame cut at the maximum is emitted; its check block marks it.

| Group | Block | Ports | Params |
|---|---|---|---|
| iq | `mix` | iq → iq | pinned (T-086): `offset_hz` (hot) |
| iq | `lowpass` | iq\|real → same | pinned (T-086): `cutoff_hz`, `transition_hz`, `stopband_db` |
| iq | `resample` | iq\|real → same | pinned (T-086): `output_rate_hz` (≤ input rate: decimation only, the DDC stages), `bandwidth_hz`, `stopband_db` |
| iq | `fm_demod` | iq → real | pinned: `deviation_hz` (hot), `output_rate_hz`, `deemphasis_s` |
| iq | `am_demod` | iq → real | pinned (T-086): `mode` (`normalized`/`envelope`, hot), `time_constant_s` (hot) |
| iq | `fsk_demod`, `msk_demod` | iq → real | pinned (T-086): `deviation_hz` (fsk) or `symbol_rate_bd` (msk, deviation = rate/4), `offset_hz`, `offset_tracking_s`, all hot |
| iq | `ppm_demod` | iq → frames (+ diagnostic `soft` soft) | pinned: `bit_rate_bd`, `chips_per_bit`, `preamble` + `preamble_chips` (chip pattern), `min_snr_db` (hot), `frame_bits` (max), `length_from` |
| iq | `subcarrier` | real → iq | pinned: `carrier_hz`, `bandwidth_hz`, `output_rate_hz`, `reference {pilot_hz, multiple, pll_bandwidth_hz}`, `phase_tracking` (hot) |
| symbol | `clock_recovery` | iq\|real → soft (+ diagnostic `timing_error` real) | pinned: `symbol_rate_bd`, `pulse`, `algorithm`, `soft_from`, `loop_bandwidth` (hot), `max_deviation_ppm` |
| symbol | `slicer` | soft → bits | pinned: `threshold` (hot), `invert` (hot) |
| symbol | `diff_decode` | bits → bits | pinned: `mode` (hot) |
| symbol | `nrzi` | bits → bits | pinned (T-086; `direction` T-108): `mode` (`transition-is-0`/`transition-is-1`, hot), `direction` (`decode`/`encode`: the running level, for MSK whose data are the coherent chips, ACARS) |
| symbol | `manchester` | soft\|bits → bits | pinned (T-086): `convention` (`thomas`/`ieee`, hot), `align` (`auto`/`fixed`, hot) |
| framing | `sync_search` | bits → frames | pinned: `mode` (`sync-word`/`offset-words`) + per-mode keys; sync-word: `frame_bits` (fixed, or the maximum), `bit_order` (`lsb` reverses 8-bit characters), `polarity` (`normal`/`either`: the complemented word also syncs and complements its frame; T-108), `length_from`, `terminator` |
| framing | `assemble` | frames → frames | pinned: `word_bits`, `start {bit, value}`, `idle_words`, `header`, `slot`, `payload`, `max_words`, `span_frames` (POCSAG address + message codewords → one message, across batches) |
| framing | `deframe` | bits\|frames → frames | pinned (T-087): `frame_bits` (fixed, or the maximum), `offset_bits`, `length_from`, `terminator` |
| framing | `interleave`, `deinterleave` | frames → frames | pinned (T-087): `depth` (column interleaver) or `permutation` (per period) |
| fec | `crc` | frames → frames | pinned: RevEng model (`width, poly, init, refin, refout, xorout`), `span` or `blocks {data_bits, check_bits, offsets}`, `strip`, `drop_invalid` (hot), `correct_burst_bits`, `synced_correction {burst_bits, lock_blocks, unlock_run}` (T-210: block mode; corrects a block's unique ≤`burst_bits`-bit burst only while the lattice is synced, outside the random-block bound, and marks the frame `corrected`) |
| fec | `bch` | frames → frames | pinned (T-087): `word_bits`, `n`, `k`, `poly` (degree n − k), `parity` (none/even/odd), `correct_bits`, `drop_invalid` (hot) |
| fec | `parity` | frames → frames | pinned (T-087): `unit_bits`, `parity`, `position` (first/last), `span`, `strip`, `drop_invalid` (hot) |
| fec | `checksum` | frames → frames | pinned (T-087): `algorithm` (sum/xor/ones-complement), `unit_bits`, `width`, `endianness`, `init`, `complement`, `span`, `strip`, `drop_invalid` (hot) |
| parse | `fields` | frames → frames | pinned: `map` (hot) |
| parse | `text` | frames → frames | pinned: `name`, `key`, `address[]`, `chars[]`, `segments`, `chars_per_segment`, `reset_on`, `terminator`, `emit` (hot), `charset` |
| parse | `consensus` | frames → frames | pinned (T-210): `key`, `fields[] {field, address}`, `clean_weight`, `corrected_weight`, `commit_weight`, `window` — a value passes only once agreeing observations reach `commit_weight` in the slot's window (a CRC-valid frame weighs `clean_weight`, a corrected one `corrected_weight`); every other value is withheld (node value cleared, `error` set) |
| multi | `follow_hops` | frames → frames | pinned: `dedupe_s` (hot), `order_window_s` |
| util | `identity` | any → same | none (implemented: the contract example) |

### 1.6 Wrapping existing code

Blocks are **adapters over the existing kernels**, not rewrites:
- **mix/lowpass/resample:** hk-dsp `filter::Nco`, `design_lowpass` + `kernels`, and the DDC's polyphase stage. hk-dsp's `Ddc` wants a `ProvenanceHandle`, which `ChunkMeta` doesn't carry, so T-086 may expose a provenance-free resampler additively.
- **fm_demod/subcarrier/clock_recovery:** the hk-demod discriminator (`dsp`), `pilot::PilotPll` and `rds::demod`'s biphase timing.
- **fsk_demod/slicer:** hk-demod `fsk`.
- **sync_search:** hk-estimate `framing::sync` and hk-demod `rds::block` syndromes. **crc:** hk-estimate `framing::crc::BitCrc` (the RevEng model over bit ranges; its whole-byte path is `CrcCore`).

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
  "schema": "hackriff.recipe", "schema_version": 2,
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
  - **Mapping** (T-111): `{frame_model, identity?: {scheme, field, format: hex | dec}, metadata[], content[], require[], service?}`. Validation checks the scheme (a known scheme, or a token read as `other:<scheme>`), the format, that at least one field is mapped, and that `service` is a token. Values are the fields' decoded values; keys are the last path segment (the whole path when two mapped paths share it). The identity takes its scheme's canonical form (`adsb-icao` 6 lower-case hex digits, `rds-pi` 4 upper-case).
  - **Rows** (T-111): one Decode row per CRC-valid frame whose map fit (`ok`/`partial`) and that has every `require`d field (default: any mapped field). `decoder_id` is `recipe:<id>`, `decoder_version` the recipe version.
  - **Ingestion is the plugin path** (T-111, `hk-pipeline/src/recipes/messages.rs`). The pipeline thread `try_send`s each accepted frame's time, channel and `Arc` layer tree onto a bounded queue (1024; full → dropped and counted in `stats.decodes_dropped`; no allocation, no blocking). A writer thread per output maps, sanitises under the pipeline's effective class with the `output_policy` parsed by the plugin manifest rules (`hk_plugins::output_metadata_policy` + `sanitize_decode`), and stores through `hk_plugins::Ingest::store_decode`: repository content gate, identity sighting with the target emitter as context, republish on `decodes/<pipeline>/<output>`. New emitters get the family step (`classify_decoder_emitters`, shared with plugin chains) with `service` (or the recipe id) as decoder evidence. Like plugin decodes, rows are not deduplicated; entity resolution folds repeated identities.
- `stage`: a named default stage stream. Any port can still be tapped ad hoc.

**Input:** `iq` means the runtime down-converts the target (emitter, selection, band, refined channel) to `sample_rate_hz`. `bits`/`soft`/`frames` means the recipe tail runs over a recorded decoded stream (§14.7): the parser-authoring loop without RF.

### 2.3 Hot edit without stopping capture (ADR-0001)

A running pipeline = one ring reader + its channel DDC + an instantiated node graph at recipe revision `(version, edit_rev)`. An edit is a whole new recipe document (the draft); `hk_recipe::EditPlan::between(old, new, catalogue)` classifies each node from the block descriptors (which params are hot, which name field maps):

| Change | Effect at the swap |
|---|---|
| Unchanged | Instance moves over with its state. |
| Params only, all changed keys `hot` | `update_params` on the instance at the chunk boundary; state kept. |
| Params only, any cold key | Rebuilt: fresh instance, `init`. |
| Block, version or inputs changed; node added | Rebuilt. |
| Node removed | Dropped. |
| Surviving node downstream of a rebuilt/added/cold node | `reset()` and a `RESET` chunk flag. |
| Field map changed | Every node whose `field-map` param names it is a `Params` change on that key (hot by definition): `update_params(params, ctx)` with the new maps at the swap. No DSP state touched, and downstream nodes (e.g. `text` assemblers) don't reset. |
| `input` changed | Channel re-plumbed, every node rebuilt (a retune-like edit, still no capture stop). |
| Outputs changed | Removed outputs' streams finish; new ones are offered. Unchanged outputs keep their consumers and `seq`. |

**Mechanics** (T-088):
1. **Validate the draft.** An invalid draft is refused (`400` with paths), and the running revision is untouched.
2. **Build off the real-time thread.** New and rebuilt instances are made with `build` + `init` on a control thread, while the old graph keeps processing. Port negotiation is re-run there too: if a rebuilt node's output `PortInfo` (`rate_hz`, `max_items`) changed, every downstream node is re-`init`ed and its output buffers re-sized on the control thread (on a staged copy, or rebuilt when it can't be re-initialised in place) **before** the swap, so the pipeline thread never allocates or designs filters.
3. **Swap.** At the first chunk boundary after the build completes, the pipeline thread exchanges graphs in O(nodes). The ring reader's cursor is untouched, so **no sample is lost** and capture never pauses.
4. **Report.** The result names `applied_at_sample`, the new `edit_rev` and the plan. An `edit` record (§14.3) marks the boundary on the pipeline's streams, and every later frame carries the new `edit_rev`.

Hot edits don't save. `POST /api/pipelines/{id}/save` writes the running revision as the recipe's next version.

### 2.4 Versioning, save, list, re-run, matching

- `schema_version` is the format version (2). An unknown version is refused; new optional keys need a new schema version, because unknown fields are errors.
  - **2** (T-085 review, 2026-09-15, before anything shipped): variable-length framing (`length_from`, `terminator`, `bit_order` on `sync_search`; `ppm_demod` → frames; `assemble`), field-map `char_bits: 4` + `pocsag-bcd`, `parity`, `skip_bits`, `scale`/`add`/`value_unit`. Version 1 was never released and is not read.
  - T-111 (2026-09-15, still before anything shipped) adds the optional `decode.service` key to version 2 instead of bumping it: no version 2 document had been released, and existing documents stay valid.
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
  - All instances feed the single `follow_hops` node, which merges frames in frame-end order (the watermark of the chunk a frame completes in; ties by start then channel) within `order_window_s` and drops duplicates within `dedupe_s`. End order (T-107) keeps the `order_window_s` bound for frames longer than the window (e.g. multi-batch POCSAG); consequently `sample_index` (frame start) need not increase across channels when a long frame overlaps short ones. Everything downstream runs once.
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
| `type` | `uint`, `int` (two's complement), `enum` (+ `values: {"0": "A"}`), `ascii` (+ `charset` ascii/latin1/rds/pocsag-bcd, `char_bits` 8/7, or 4 with `pocsag-bcd`; `parity` odd/even/ignore: the character's top bit after `bit_order` is masked out and checked, a failure renders U+FFFD with a `parity` fit error; a trailing partial character of a `remainder` field is padding), `bitfield` (+ `flags: [{name, bit}]`, bit 0 = first bit on air), `bytes` (uninterpreted), `layer` (+ `fields`) |
| `offset` | From the enclosing layer's start, in `unit`; omitted = after the previous *present* sibling |
| `length` | A number of `unit`s; `"remainder"`; or `{field, scale, add}` (a length field). Integers are ≤ 64 bits and need a fixed length. |
| `unit` | `bits` or `bytes` (override) |
| `endianness` | `big` or `little`: byte order of whole-byte integers |
| `bit_order` | `msb` or `lsb`: sub-byte fields and characters (POCSAG 7-bit LSB-first text) |
| `condition` | `{field, eq\|ne\|lt\|le\|gt\|ge\|in}` combinable with `{all:[…]}`, `{any:[…]}` and `{not:…}`. False = absent, not an error. |
| `repeat` | A count (fixed, `{field,…}` or `"remainder"`); instances addressed `name[i]` |
| `skip_bits` | `uint`/`int`: bit positions (0 = the field's first bit) left out of the value, the rest concatenated (ADS-B altitude without its Q bit; POCSAG RIC = address ‖ slot around the function bits) |
| `scale`, `add`, `value_unit` | `uint`/`int`: the node's `value` is `raw × scale + add` and `text` shows it with `value_unit` (ADS-B `25`, `-1000`, `ft`), so the UI does no arithmetic |
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
  - `seq`, `t_ns` (time of the first bit, integer Unix nanoseconds; `t` before stream contract 1.2, T-354), `content_class`, `gated`, `crc_status`, `decoder` (`recipe:<id>@<version>`), `frame_model`, `emitter_id`;
  - `metadata {frame, sample_index, channel, channel_hz, bit_len, recipe_version, edit_rev, fec_corrected_bits, fit}`;
  - `content {hex, layers}`.
- **Interleaved records:** `status` (one per ~250 ms tick, all nodes batched) and `edit` (hot-edit boundary).
- **Why messages, not a new binary kind:**
  - A new `kind` value is a major change under §1; a new message record type is a minor one, and pre-1.2 readers skip it.
  - The §6 message gate (content gated; metadata reduced to the allowlist under restricted classes) is exactly the needed legal behaviour, already tested.
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
7. `crc` `crc` (block mode, per-position offsets, strip → 64-bit frames with `crc_status`; T-210 `synced_correction`: ≤2-bit bursts corrected only while block-synced, those groups `corrected`, never `valid`);
8. `group` `fields` (map `rds_group`);
9. `agree` `consensus` (T-210: key `pi`, fields `tp`, `pty`, `ps.chars` by `ps.segment`, `radiotext.chars_a`/`chars_b` by `radiotext.segment`; clean 2, corrected 1, commit 3, window 8 — two agreeing groups with at least one CRC-valid, or three corrected);
10. `ps` `text` (PS: key PI, address `ps.segment`, 4 × 2 chars);
11. `rt` `text` (RadioText: address `radiotext.segment`, chars `chars_a` for 2A or `chars_b` for 2B, 16 segments, reset on the A/B flag, terminator 0x0D).

**Field map `rds_group`** (bits):
- `pi` 0–16 (hex), `group_type` 16–20, `version` 20 (enum A/B), `tp` 21 (bool), `pty` 22–27 (numeric: RDS and RBDS name tables differ);
- layer `ps` 27–64 when `group_type == 0`: bitfield `flags` {ta, music, di}, `segment` (2), `block3` (16, AF or PI), `chars` (16, RDS charset);
- layer `radiotext` 27–64 when `group_type == 2`: `ab`, `segment` (4), then `chars_a` (32) if `version == 0`, else `pi_repeat` (16) + `chars_b` (16).

**Outputs:**
- `groups` (inspector, from `group`: every group as decoded, `valid`, `corrected` or `invalid`);
- `group-info` (messages, from `agree`: identity `rds-pi` from `pi`; metadata group_type, version, tp, pty — only CRC-valid frames become rows, and only consensus-committed values are present);
- `station` (messages: PS text, identity from `ps.key`);
- `radiotext` (messages);
- stage `mpx` (spectrum view) and `symbols` (the eye/soft symbols).

**Policy and refinement:** `unrestricted` (broadcast RDS is public station identity, as in hk-demod); `refine` minimises `crc.error_rate` over `center_hz`.

**Oracle tie-in.** The test asserts the recipe's polynomial, offset words (and their block positions) and bit rate equal `hk_demod::rds` constants. T-094 then asserts the recipe's decoded PI/PS/PTY/RT equal the existing decoder's on `fm_100p8M` through the mock SDR.

### 5.1 Skeletons: POCSAG, ACARS, ADS-B

`recipes/{pocsag,acars,adsb}.recipe.json` prove the contracts express the other three tutorials; `crates/hk-blocks/tests/m1_recipes.rs` validates them against the pinned catalogue (placeholder blocks may warn, nothing errors). Their tutorial tasks (T-095…T-097) tune them.
- **POCSAG** (follow-hops): `fsk_demod` → `clock_recovery` 1200 Bd → `slicer` → `sync_search` (0x7CD215D8, 512-bit batches) → `bch` → `assemble` (32-bit words; start = bit 0 is 0; idle 0x7A89C197; header 20 bits, 3-bit slot, 20 payload bits per word; across batches) → `follow_hops` → `fields`. Assembly sits upstream of the merge, so a message never mixes channels. The map reads `ric` (23 bits, `skip_bits` [18, 19]), `function`, then `numeric` (`pocsag-bcd`, LSB first) or `alpha` (7-bit, LSB first). `restricted-paging` with the paging allowlist.
- **ACARS** (conventions from acarsdec's receiver source, T-108; see [docs/tutorials/03-acars.md](../tutorials/03-acars.md)): `am_demod` → `subcarrier` (1800 Hz) → `msk_demod` → `clock_recovery` 2400 Bd → `slicer` → `nrzi` (`direction: encode`, transition-is-0: the data are the coherent MSK chips and a 2400 Hz tone means an unchanged chip) → `sync_search` (`+* SYN SYN SOH` = 0xD554686880 in air order, `polarity: either`, `bit_order: lsb`, terminator ETX 0x83 / ETB 0x97 at 8-bit steps + 16 trailer bits) → `crc` (CRC-16/KERMIT over the parity-bearing characters after SOH, BCS low byte first) → `fields` (7-bit + odd parity characters).
- **ADS-B**: `ppm_demod` (2 Msps, preamble 0xA140 over 16 chips, `length_from` DF → 56/112) → `crc` (CRC-24 0xFFF409 over the frame) → `fields` (DF, ICAO, ME: identification, airborne position with altitude = AC12 without Q × 25 − 1000 ft, velocity in kt and ft/min).

## 6. Crate placement

| Crate | Holds | Depends on | Why |
|---|---|---|---|
| `hk-recipe` (new) | `Recipe`, `NodeSpec`, `OutputSpec`, `PortType`, `ParamSchema`/`ParamType`, `BlockDescriptor`, `Catalogue`, `FieldMap`, `EditPlan`, validation | hk-model, hk-stream, serde, serde_json | Pure data, no DSP. hk-api, hk-store (captures) and hk-estimate (authoring assist emits field-map/param suggestions) use it without linking the block library. |
| `hk-blocks` (new) | `Block`, `Io`, `PortInfo`, buffers, `Status`, `BlockFactory`, `Registry`, catalogue, all block implementations | hk-recipe, hk-dsp, hk-estimate, hk-demod, hk-stream, hk-model | Blocks wrap kernels from three DSP crates. Putting them in hk-dsp would invert dependencies (hk-dsp can't see hk-demod); putting them in hk-pipeline would force every block test to compile the whole composition. The DSP dependencies are pre-added so T-086/T-087 never touch the manifest. |
| `hk-stream` (`inspector` module) | Frame/status/edit record types, `LayerTree`, `byte_span`, hex helpers | (existing) | The wire contract and its gate live here, like the `audio` and `bursts` profiles. |
| `hk-pipeline` (T-088) | Recipe runtime: graph build/swap, runner, channel DDC, taps, publishers, recipe store, chain-budget kind | + hk-blocks, hk-recipe | Composition belongs here (ADR-0001 S1 chains). |
| `hk-api` (T-088/T-089/T-091/T-092) | Routes over the runtime and store | + hk-recipe | Thin HTTP over the above. |

The `hk-recipe`/`hk-blocks` dependencies of hk-api, hk-pipeline and hk-estimate, and `hk-stream` for hk-store, are pre-added (§7).

## 7. Parallel-work map

File ownership so T-086…T-093 run in separate worktrees without collisions. T-085 **pre-added** every shared declaration a task would otherwise edit: Cargo dependencies, `pub mod` lines, empty stub modules and the API dispatch chain. Tasks fill in only their own files.

| Task | Owns (writes) | Reads / codes against | Notes |
|---|---|---|---|
| **T-086 Blocks A** | `crates/hk-blocks/src/blocks/iq/**`, `crates/hk-blocks/src/blocks/symbol/**`; additive `pub` helpers in hk-dsp (`filter`, `ddc` resampler) and hk-demod (`dsp`, `pilot`, `rds::demod` `drain_into`) | `block.rs`, `buffer.rs`, `status.rs`, `schema.rs` (`frame_length`), catalogue test | Pins placeholder params in its own `planned()`. `ppm_demod` implements `length_from`. **Coordinate with T-084** in hk-demod `rds` (T-084 fixes RDS HIL findings): additive new functions only, no edits to T-084's lines. |
| **T-087 Blocks B** | `crates/hk-blocks/src/blocks/framing/**` (incl. `assemble`), `crates/hk-blocks/src/blocks/fec/**`; additive `pub` in hk-estimate `framing::{crc,sync}` | as above; hk-demod `rds::block` (read-only) | Unit tests: RDS offset words + 0x5B9, POCSAG 0x7CD215D8 + BCH(31,21) + message assembly, CRC-16, CRC-24, `length_from`/`terminator`/`bit_order`. |
| **T-088 Recipe runtime** | `crates/hk-pipeline/src/recipes/{runtime,graph,swap,taps,store,openers}.rs` (stubs exist); one `recipe` kind in `chains/budget.rs`; `crates/hk-api/src/recipes.rs` (stub exists, dispatched); its `ROUTES` rows; `docs/api.md` "Recipes and pipelines" subsection; `crates/hk-recipe/src/edit.rs` extensions | hk-recipe, hk-blocks contract, `hk_stream::inspector` types | Publishes frames through a `FrameSink` trait in `recipes/` until T-089's `Publisher::publish_frame` merges, then swaps to it (one-line change). Allocation-free runner test. |
| **T-089 Parser + inspector API** | `crates/hk-recipe/src/fields/eval.rs` (stub exists; `fields/mod.rs` holds the schema, unchanged); `crates/hk-blocks/src/blocks/parse/**`; `crates/hk-stream/src/inspector.rs`, and `publish_frame`/`publish_record` plus the optional `inspector` header field in `publisher.rs`/`header.rs` (reviewed: legal gate); bump `STREAM_VERSION_MINOR` to 2; `crates/hk-api/src/inspector.rs` (stub exists, dispatched: `POST /api/captures/{id}/parse`); its `ROUTES` rows; `docs/api.md` "Inspector" subsection; `docs/stream-contract.md` §14 finalisation | hk-recipe schema | Contract tests in `crates/hk-cli/tests/api_contract.rs` under its marker. |
| **T-090 Inspector UI** | `ui/src/inspector/**` | `docs/api.md`, §14 | Thin client: renders `nodes`, `bytes`, `byte_index` verbatim. |
| **T-091 Authoring assist** | `crates/hk-estimate/src/assist/**` (stub exists; reuses `framing::search`); `crates/hk-api/src/assist.rs` (stub exists, dispatched); its `ROUTES` rows and `docs/api.md` "Assist" subsection | hk-recipe (emits `FieldMap` fragments and `sync_search`/`crc` param objects as suggestions with scores) | `hk-recipe` is already an hk-estimate dependency (no cycle: hk-recipe depends on no DSP crate). |
| **T-092 Decoded capture + scrub** | `crates/hk-store/src/decoded.rs` (stub exists: §3-stream capture writer/reader, quota); `crates/hk-pipeline/src/recipes/capture.rs` (stub exists); `crates/hk-api/src/captures.rs` (stub exists, dispatched: list/frames routes); `ui/` scrub hooks | §14.7, `hk_stream::inspector`, `StreamReader` | Depends on T-088 (codes against its runtime). |
| **T-093 follow_hops** | `crates/hk-blocks/src/blocks/multi/**`; `crates/hk-pipeline/src/recipes/hops.rs` (stub exists); hk-core only if a multi-channel reader primitive is needed | §2.5 | Depends on T-088. |
| **T-094…T-097 Tutorials** | `recipes/<name>.recipe.json`, `tests/e2e/tests/acceptance/m1_<name>.rs`, `docs/tutorials/` | everything | T-094 edits `recipes/rds.recipe.json` only to tune params and bumps `version` for any semantic change; T-095…T-097 start from the §5.1 skeletons. |

**Pre-added (final; don't edit):**
- Cargo: `hk-recipe` in hk-api, hk-pipeline and hk-estimate; `hk-blocks` in hk-pipeline; `hk-stream` in hk-store. Workspace `Cargo.toml` and `crates/hk-blocks/Cargo.toml` are complete.
- `pub mod` lines: `hk-api/src/lib.rs` (`recipes`, `inspector`, `assist`, `captures`), `hk-pipeline/src/lib.rs` (`recipes`) and `recipes/mod.rs` (every file above), `hk-estimate/src/lib.rs` (`assist`), `hk-store/src/lib.rs` (`decoded`), `hk-recipe/src/fields/mod.rs` (`eval`), `crates/hk-blocks/src/blocks/mod.rs` (`register_all` already calls every group).
- API dispatch: `hk-api/src/http.rs` already chains `recipes::route`, `inspector::route`, `assist::route` and `captures::route` (each stub returns `None`).

**Shared append-only (can't be pre-stubbed; append in your own block, rebase conflicts are textual and trivial, merge in task order):**
- `crates/hk-api/src/http.rs::ROUTES`: rows under your task's marker comment.
- `crates/hk-cli/tests/api_contract.rs`: test fns under your task's marker comment.
- `docs/api.md`: your own subsection (and move your rows out of "Decoder workbench (planned)").
- **Opener registration** (`crates/hk-cli/src/pipeline.rs` `OpenerRegistry::with` chain, plus the `PipelineHandle` accessor in `crates/hk-pipeline/src/run.rs`): stubbing it would advertise openers that refuse everything. T-088 appends one line each for `stage` and `inspector`; T-089 (capture re-parse) and T-092 (capture replay) extend the `inspector` opener from their own files and add no registration line.

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
3. ~~**Status cadence.**~~ Settled (T-085 review): one `status` record per ~250 ms tick for the whole pipeline, every node batched as `<node>.<metric>` keys (§1.3, stream contract §14.3).

## 8. Amendment — audio output: a sink block, an `audio` output kind, a live-edge policy (T-221, 2026-09-15, from the user)

**Status:** PROVISIONAL, planning only, no code. Source: [docs/15 §10](../15-decoder-synthesis.md) ("Listen is just a decode pipeline with an audio sink"), written by the user after live testing. §§1–7 stand unchanged. This section adds what a recipe needs in order to *be* an audio decoder; [ADR-0015 §12](0015-decoder-synthesis-contracts.md) says how today's Listen path migrates onto it without going dark.

**The change in one line.** A recipe may end in an **audio sink**, and its output is the audio stream the product already serves — so "listen to this" and "decode this" are the same kind of object, differing only in the sink.

### 8.1 What is actually missing

Not the stream. `kind: "audio"` already exists in the wire contract (ADR-0004, C24, stream contract §12.2), already counts as content (`StreamKind::payload_is_content`), and a `real` port opened as a **stage** stream is already carried as `kind: "audio"` (§14.4). But a stage tap is a *waveform tap*, not listenable audio: `rf32_le` at the port's own rate, with no 48 kHz/`ri16_le` conversion, no 20 ms framing, no squelch/AGC/level status records and no audio header profile. Listen's profile (§12.2) has all of that.

So what is missing is **the sink and the output kind that binds a node's `real` port to the existing audio profile** — plus, less obviously, a liveness rule (§8.5).

### 8.2 The `audio` output kind

A fourth `outputs[]` kind beside `inspector`, `messages` and `stage` (§2.2):

```jsonc
{ "id": "audio", "kind": "audio", "from": "out",          // a node whose output port is `real`
  "channels": "mono",                                      // the only value schema 3 accepts (§8.4)
  "profile": { "mode": "wfm", "deemphasis_s": 75e-6 } }    // header hints; measured values win
```

- **Validation:** `from` resolves to an `audio_out` node (§8.4); at most one `audio` output per recipe (the pipeline's liveness and listener budget are per pipeline, §8.5/§8.8); `channels` is `mono`.
- **No new stream kind, no new port type.** The stream served is the §12.2 audio profile **unchanged** in `kind` (`audio`), `datatype` (`ri16_le`), `sample_rate_hz` (48000) and `frame_samples` (960), with the same type-1 data records and type-3 status records. Stream id `audio/<pipeline>/<output>`, beside `inspector/<pipeline>/<output>`.
- **Header, additively** (a 1.x minor stream-contract change under §1 of that document): the `audio` object gains `pipeline_id`, `recipe` (`<id>@<version>`), `output_id`, `edit_rev`. Every existing key — `mode`, `mode_confidence`, `mode_rules`, `params`, `snr_db`, `squelch`, `agc`, `deemphasis_s`, `demod`, `refinement` — keeps its name and meaning, because the UI's `AudioSession`, `py/examples/hk_audio_wav.py` and any TCP consumer already parse them.
- **Status records** keep the §12.2 audio keys (`level_dbfs`, `snr_db`, `squelch_open`, `agc_gain_db`, `frames`, `squelched_frames`, `lost_samples`, `latency_ms`, `backlog_s`) **and** carry the §1.3 per-node `<node>.<metric>` batch on the same tick. One record, two vocabularies: the audio keys are what a dock meter reads, the node keys what the workbench reads.

### 8.3 Gating: audio is content, and it is gated twice

1. **Egress** (§6, unchanged): an `audio` payload is withheld (`GATED`, header only) under a class that forbids content, exactly as for `bits`/`symbols`/`iq`.
2. **Before any ring read.** Listen refuses *before* it attaches anything (`hk_pipeline::chains::listen::listen_class`), and that rule is a property of **the target band and the source class**, not of the Listen chain — `open/iq` already reuses it (§12.3). Therefore: **a recipe declaring an `audio` output runs `listen_class` on its target at pipeline start**, and a refusal is the start's refusal, before the channel DDC exists.

`output_policy` (§2.6) still clamps and can only restrict. A recipe that declares an `audio` output **and** a `content_class` that forbids content is a validation error (it would serve permanently empty audio) — the same shape of rule as §2.6's mandatory `metadata_keys`.

### 8.4 Catalogue additions (group `audio`)

Additive §1.5 entries; each pins its descriptor in `planned()` and is enforced by the existing drift test.

| Block | Ports | Params | Wraps |
|---|---|---|---|
| `squelch` | real → real | `mode` (`snr`/`fm-noise`), `open_snr_db`, `hysteresis_db`, `attack_s`, `hang_s` — all hot | `hk_demod::audio` squelch (C19 §"Squelch") |
| `agc` | real → real | `enabled`, `target_dbfs`, `max_gain_db`, `attack_s`, `decay_s`, `hang_s` — all hot | `hk_demod::audio` AGC (C19 §"AGC") |
| `deemphasis` | real → real | `tau_s` (hot) | the existing single-pole filter (today a `fm_demod` param; both stay) |
| `audio_out` | real → **(sink, no output port)** | `output_rate_hz` (48000), `datatype` (`ri16_le`), `frame_samples` (960), `loudness_target_dbfs` | resampler + `hk_stream::audio::encode_pcm` |

- `audio_out` is the catalogue's **first sink**: an input and no outputs. §1.1's port table is unchanged; §2.2's validation learns that an `audio` output's `from` names an `audio_out`, and that a sink node may be a graph leaf.
- **A closed squelch emits no items, not silence.** `squelch` produces zero items while closed and flags the first chunk after re-opening `DISCONTINUITY`. That is exactly how Listen expresses a gap today (a jump in `sample_index`), so wire behaviour is preserved and a long silence costs no bandwidth. Consequence for §1.4 rule 3: a block may legally emit nothing for an unbounded time; the `hold_items` latency bound governs only the open path.
- **Deliberately not invented here.** There is **no `ssb_demod` and no `cw_demod`**, and Listen serves `usb`/`lsb`/`cw` today. Writing them properly (carrier estimate, raster snap, clarifier — C19 §"SSB carrier") is real DSP work this amendment does not fund. Their absence is exactly why the migration is **per-mode** (ADR-0015 §12.5) rather than all-at-once. **Stereo** is also out: `stereo_decode` would need a block *and* a wire change (`channels`, interleaved `ri16_le`), and no client asks for it.

### 8.5 Live-edge policy — the real-time difference

A recipe pipeline is a **throughput** reader: if it falls behind, the ring laps it, `lost_samples` rises and the next chunk carries `DISCONTINUITY` (§1.4 rule 4). Audio has a stronger requirement — it must **stay live**. Listen skips forward to the live edge rather than playing out a growing backlog, and holds ≈ 0.6 s of per-consumer queue.

A recipe with an `audio` output therefore declares, and the runtime enforces:

```jsonc
"input": { "…": "…", "liveness": { "mode": "live-edge", "max_backlog_s": 0.6 } }
```

- **`live-edge`** (the default for an audio output): when the reader's backlog exceeds `max_backlog_s`, the runtime **seeks the reader to the live edge**, counts the skip and flags `DISCONTINUITY`.
- **`throughput`** (the default for every other recipe, i.e. today's behaviour): never seeks.

This is the single most likely way a naive port would ruin live listening: everything would still decode, it would just lag, and no existing test asserts otherwise. **Latency is a contract for audio and merely a statistic for decoding.**

### 8.6 Schema version 3

Output kind `audio`, `input.liveness` and `refine.objective.builtin` (§8.7) are new optional keys, and unknown fields are errors, so `schema_version` goes **2 → 3** under §2.4's rule. ADR-0015 §2.3's `refine.objective.evidence` bumps to the same 3: **one bump, three keys**, landing together or reserving each other's names.

### 8.7 Refinement objectives that are not a node metric

§1.3 declares `refine.objective` as `{node, metric, goal}`. Listen's actual objective (T-070, `hk_demod::refine::WfmObjective`) is not that shape: it measures 19 kHz pilot C/N₀ against MPX guard bands, an ITU 99 % occupied-bandwidth floor and an RDS validation pass, over the **channel IQ window** — not over one node's output port. Forcing it into `{node, metric}` would replace a working objective with a weaker one. So `refine.objective` gains a third form:

```jsonc
"refine": { "objective": { "builtin": "wfm-pilot" }, "tune": ["center_hz", "bandwidth_hz"] }
```

`builtin` names a registered `hk_demod::refine::Objective` (`wfm-pilot` today; the NBFM/AM audio-SNR and FSK CRC-rate objectives are T-070's hook rows). **The loop does not move**: `hk_pipeline`'s refinement loop owns it, as it already does, and one loop now serves all three forms — `{node, metric}` (`crc.error_rate → min`), `{builtin}` (`wfm-pilot`) and later `{evidence}` (ADR-0015 §2.3).

### 8.8 Budget

An audio-output pipeline is admitted as a **listener**, not merely as a `recipe` chain: it counts against `max_listeners` (8) as well as `max_chains`, costed like a Listen chain from the tuned sample rate (stream contract §12.1, T-066/T-071). Otherwise moving Listen onto recipes would silently delete the listener cap that protects the capture path.

### 8.9 Worked example (`recipes/analog-wfm.recipe.json`, planned)

`input` `iq` 240 kS/s → `fm` `fm_demod` (75 kHz deviation, 75 µs de-emphasis) → `sq` `squelch` (`fm-noise`) → `gain` `agc` → `out` `audio_out`; `refine.objective = {builtin: "wfm-pilot"}`; `outputs`: `audio` from `out` — **and**, hanging off the same `fm` node, §5's RDS chain with its `messages` outputs.

That one document is the whole point of the amendment: **one pipeline, two outputs, audio and RDS as siblings**, where today they are a Listen chain and an unrelated `analog-auto` decode path that the UI reconciles by hand.

## 9. Amendment: the coverage catalogue, what a PSK demodulator emits, and two-shape FEC (T-606, 2026-09-23)

**Status:** PROVISIONAL, contract only. No block is implemented, and no ADR status changes. Source: the T-554 disposition audit, [docs/18 §§6–8](../18-decoder-coverage.md) (§7 is the ranking behind every row here; §8 is the delta this section absorbs). §§1–8 stand unchanged. §8 added the audio sink; this section adds the blocks that 25 of the audit's 36 native families are waiting on, and answers the one real contract question they raise.

**Code:** port shapes are pinned in `hk_blocks::catalogue::mauto_rows()` (`crates/hk-blocks/src/blocks/{iq,symbol,fec}/mauto.rs`), which `planned()` includes, so the existing drift test (`implemented_blocks_match_their_pinned_descriptors`) holds every implementation to its row. Parameters are **placeholders** (`params_pinned: false`: accepted unchecked, with a warning) until each block's ticket pins them. `crates/hk-blocks/tests/mauto_catalogue.rs` asserts every row's ports, asserts that `fec` has exactly the two shapes §9.3 names, and type-checks one chain per use case (SIGNAL-034, SIGNAL-004, SIGNAL-080, SIGNAL-053, SIGNAL-054) against the catalogue.

### 9.1 Catalogue rows (additive to §1.5)

| Group | Block | Ports | Parameter sketch (placeholder; the ticket pins it) | Ticket |
|---|---|---|---|---|
| iq | `psk_demod` | iq → soft (+ diagnostic `symbols` iq, `timing_error` real) | **pinned by T-609:** `modulation` (`bpsk`/`dbpsk`/`qpsk`/`oqpsk`/`dqpsk`/`pi4-dqpsk`/`8psk`/`d8psk`), `symbol_rate_bd`, `pulse` (`rrc`/`rect`/`half-sine`; the last two OQPSK-only), `rolloff` (0.05–1), `mapping` (`gray`/`natural`; OQPSK Gray only), `rotation_deg` (hot), `iq_swap` (hot), `loop_bandwidth` (hot; replaces the sketch's two loop bandwidths, since liquid's `symtrack` has one knob), `max_offset_hz` | T-609 (**implemented**) |
| iq | `css_demod` | iq → soft | `spreading_factor`, `bandwidth_hz`, `ldro`, `sync_word`, `header` (`explicit`/`implicit`) | unfiled |
| iq | `ssb_demod` | iq → real | `sideband`, `carrier` (`estimate`/`raster`/`fixed`), `raster_hz`, `clarifier_hz` (hot), `bandwidth_hz`, `output_rate_hz` | unfiled |
| iq | `cw_demod` | iq → real | `output` (`tone`/`envelope`), `tone_hz` (hot), `bandwidth_hz` (hot), `output_rate_hz` | unfiled |
| iq | `ofdm_demod` | **reserved name, no descriptor** (§9.2) | — | unfiled (DAB+) |
| symbol | `mlevel_slicer` | soft → bits (k bits per symbol) | `levels`, `thresholds` (`auto`/`fixed`), `fixed_levels` (hot), `mapping` (`gray`/`natural`), `invert` (hot) | T-612 |
| symbol | `descramble` | bits\|frames → same | `mode` (`additive`/`multiplicative`), `poly`, `init`, `offset_bits` (frames: the sync word is not scrambled), `output` (`serial`/`byte-lsb`) | T-608 |
| symbol | `bitstuff` | bits\|frames → same | `flag` (0x7E), `stuff_after` (5), `direction` (`destuff`/`stuff`), `abort_ones` (7) | T-613 |
| symbol | `codeword_map` | bits\|frames → same | `word_bits`, `value_bits`, `table[]` (index = value), `align` (`auto`/`fixed`), `on_invalid` (`drop`/`substitute`) | unfiled |
| symbol | `despread` | soft\|bits → bits | `chips_per_symbol`, `sequences[]` (index = value), `bit_order`, `max_chip_errors` (hot) | unfiled |
| symbol | `equalise` | iq → iq | `algorithm` (`cma`/`lms-dd`), `taps`, `samples_per_symbol`, `step` (hot), `constellation` | unfiled |
| fec | `viterbi` | soft\|bits → bits (streaming) | **pinned by T-610:** code: `constraint_length` + `polys[]` (hex, transmission order) + `poly_order` (`newest-lsb`, libfec/GNU Radio, default; `newest-msb`, the textbook octal) + `invert[]` + `puncture[]` (one `0`/`1` string per generator over the period), **or** `trellis {input_bits, output_bits, next_state[], output[]}`; `traceback_bits` (default 64), `align` (`auto`/`fixed`) | T-610 (**implemented**) |
| fec | `viterbi_frames` | frames → frames (one code block per frame) | **pinned by T-610:** the same code keys; `termination` (`terminated`/`tail-biting`/`truncated`, required), `span` | T-610 (**implemented**) |
| fec | `reed_solomon` | frames → frames | `n`, `k`, `symbol_bits`, `poly`, `fcr`, `prim`, `dual_basis`, `depth` (interleaved codewords), `strip`, `drop_invalid` (hot) | T-611 |

These are docs/18 §7's rows, with four deliberate differences:
- **`viterbi_frames` is a fourteenth row** that docs/18 does not have. It is the per-frame shape of T-610's trellis engine (§9.3), and without it `viterbi` unlocks about half of the ten families it was counted against.
- **`descramble`, `bitstuff` and `codeword_map` take `bits|frames`**, where T-608 and T-613 said `bits → bits`. A `bits` port carries no frame boundary, and the reset these blocks need is per frame:
  - CCSDS de-randomisation restarts after each ASM;
  - BLE and LoRa whitening restart each packet;
  - VDL2's AVLC stuffing sits inside RS-decoded frames (SIGNAL-004).

  Frames mode applies the same rule to a frame body, starting at `offset_bits`. This is the minimum shape that serves those families, not gold-plating. Streaming mode stays for self-synchronising scramblers (V.35) and for AIS/AX.25, whose flags pass through `bitstuff` intact so that `sync_search` still frames on them.
- **`equalise` is `iq → iq`,** placed ahead of the demodulator, not on symbols (§9.2).
- **`ofdm_demod` is named but not pinned** (§9.2).

`viterbi` also takes an explicit **`trellis`** table because the P25 Phase 1 1/2-rate and 3/4-rate trellises are finite-state codes defined by table, not binary feedforward polynomials. Without the table, SIGNAL-080 would sit on a block that cannot express its code.

### 9.2 Decision: `psk_demod` de-maps inside the block and emits one `soft` item per bit (option a)

§1.1's `soft` is "`f32` soft symbol/bit, positive = 1", one value per item. **Decision: (a).** `psk_demod`, and every de-mapping demodulator after it (`css_demod`), resolves the constellation inside the block and emits **one `soft` item per bit**, k items per symbol. **No port type is added; §1.1's table is unchanged.**

**Why (a).**
1. **It works today, and every consumer stays as it is.** `slicer`, `descramble`, `despread`, `viterbi` and everything downstream code against `soft`/`bits` as they already exist. The five use-case chains in `mauto_catalogue.rs` type-check against the catalogue with no new type.
2. **For most of the families served, the "discarded joint information" is zero.**
   - Gray-mapped QPSK/OQPSK is two independent BPSK channels on I and Q, so per-bit LLRs are *exact*, not an approximation. Coherent BPSK/QPSK/OQPSK is what CCSDS, LRPT, HRIT, HRPT, Inmarsat and Zigbee use.
   - The loss is real only where bits share a symbol non-separably: 8PSK, D8PSK and π/4-DQPSK (VDL2, TETRA). Even there, per-bit max-log LLRs are the standard BICM receiver. What they give up is the gain from iterative demapping, and nothing in this catalogue iterates: LDPC and turbo are deliberately off docs/18 §7's list.
3. **The consumers that do want the joint symbol are placed where they don't need it.** A blind equaliser runs on `iq` ahead of the demodulator (`equalise`, CMA or decision-directed on a declared constellation). A symbol-domain soft-decision equaliser, and iterative decoding, are exactly what (a) does not serve, and exactly what the future type below is for.

**The sequencing argument, and a finding it adds.** `ofdm_demod` forces the port-type question and can't take (a) as it stands. So the change is **OFDM's cost, not PSK's**, and OFDM buys exactly one native family, DAB+ (docs/18 §7, finding 2). The decision should be argued as "for DAB+". But on inspection, DAB+ does not mainly need *complex constellation points*:
- DAB's DQPSK is differential per carrier, so de-mapping is local to each cell.
- What cannot be expressed today is **soft values carrying frame boundaries**. The FIC/MSC split and the CIF structure must survive time de-interleaving and the Viterbi decoder while the values are still soft, but `soft` carries no boundaries and `frames` carries no soft values.
- The same gap is behind the ~2 dB that the burst-structured soft chains give up today by going hard-decision through `viterbi_frames` (§9.3): TETRA's per-burst descramble and de-interleave ahead of RCPC Viterbi, and P25's per-frame trellis.

So the candidates for the new type are:
- **`soft_frames`:** an f32 vector plus `FrameInfo`. It would also carry an LLR vector per symbol as a degenerate frame.
- **Complex points:** these serve coherent OFDM (DVB-T, which is a plugin) and a symbol-domain equaliser.

The DAB+ ticket decides between them, as a contract change reviewed like this ADR. It should not name the type `symbols`, which already names a wire stream kind (stream contract §5.2, §13.3) that carries the output of `soft` ports.

**The de-mapped `soft` output contract** (normative for `psk_demod`, `css_demod` and any later de-mapping demodulator):
- **Order.** k items per symbol, emitting the symbol label **MSB first**. That matches §1.1's frame packing and `mlevel_slicer`.
- **Value.** Positive = 1. Magnitude is a reliability proportional to the max-log bit LLR, **up to a common positive scale**, and consumers must not depend on that scale:
  - `slicer` at threshold 0 doesn't;
  - Viterbi's additive branch metrics don't;
  - correlation in `despread` doesn't.

  A consumer that needs calibrated LLRs is out of this contract and belongs to the future type.
- **Differential modes** (`dbpsk`, `dqpsk`, `pi4-dqpsk`, `d8psk`) are resolved inside the block, so the output is data bits, not phase changes, and it keeps soft differential detection. `bpsk` followed by the existing `diff_decode` stays expressible, as the hard-decision equivalent (Orbcomm).
- **Time map.** `rate_hz` is the **item** rate (k × symbol rate), and `source_per_item` is samples per symbol ÷ k. The physical `symbol_rate_bd` and `bits_per_symbol` go out as status `extra`s so they aren't lost. A tap on the port states `bits_per_symbol: 1` in its docs/07 `Framing`, which is literally true, since each item is one bit's decision variable. `css_demod`'s reduced-rate header symbols (SF−2 bits) make the item rate non-uniform over at most 8 symbols. The block keeps `source_per_item` at the payload rate, and the packet's first item (below) is exact.
- **Phase ambiguity.** This is a cost of (a) that the T-554 framing did not name. Coherent M-PSK locks with an M-fold ambiguity, and de-mapping inside the block turns it into a bit mapping that no downstream `bits` block can undo in general. It is resolved by the **hot** parameters `rotation_deg` and `iq_swap`, so the §1.3 refinement loop (`{node, metric}`, e.g. the ASM `sync_search` lock or `reed_solomon.error_rate`) or the MAUTO search can try the ≤ 4 rotations without losing loop state. BPSK's 180° case is also absorbed by `sync_search` `polarity: either`, and the differential modes have none.
- **Burst boundaries.** A demodulator that finds a burst or packet start in its own domain marks the **first item of each burst `DISCONTINUITY`**, the existing §1.1 flag. Examples: `css_demod` on the LoRa preamble, and `psk_demod` in burst mode. `deframe` (bits) already starts a frame after a discontinuity, so packet framing needs no new type (SIGNAL-053's chain).
- **The `symbols` diagnostic port is a presentation tap, not a data path.** It carries complex points at the symbol rate after carrier and timing recovery, for the workbench's constellation view (a `stage` output, gated as content like any `iq` stream). **A recipe must not wire it into a node input.** Otherwise option (b) arrives by the back door, unreviewed. §2.2 validation does not refuse that today, because a diagnostic port is addressable as `node.port`. **T-609 adds the refusal to `Recipe::validate`, with a test, before `psk_demod` registers.**

### 9.3 `fec` is a group with two shapes

| Shape | Blocks | Where it sits |
|---|---|---|
| **per-frame, hard decision**, `frames → frames` | `crc`, `bch`, `parity`, `checksum`, `reed_solomon`, `viterbi_frames` | after frame sync |
| **streaming, soft decision**, `soft\|bits → bits` | `viterbi` | before frame sync: the code runs continuously and the sync word is found in its output |

The test `fec_is_a_group_with_exactly_two_shapes` fails on a third, so a new shape has to be named here rather than discovered. The shapes fix the order. CCSDS is `psk_demod → viterbi → sync_search (ASM) → descramble → reed_solomon`. Putting RS straight after the streaming decoder is refused by type (`mis_ordered_fec_chains_are_refused_by_type`).

**Why `viterbi_frames` exists.** Of the ten families docs/18 counted against `viterbi`, only the continuously coded CCSDS four (telemetry, LRPT, HRIT/LRIT, HRPT) are known to decode as a stream; Inmarsat Aero is unverified either way. The others decode **one code block per frame**, after frame sync and de-interleaving:
- TETRA, P25 Phase 1 and NXDN;
- Inmarsat STD-C;
- MIL-STD-188-110, whose block interleaver is this catalogue's `deinterleave`, on frames;
- DAB+.

A `bits` port can't say where a code block starts. So:
- one trellis engine gets two descriptors, both T-610's;
- `viterbi_frames` is hard-decision (~2 dB) until the framed-soft type of §9.2 exists;
- **whichever shape is fed hard bits reports it** (a status `extra`), so hard-decision performance is never presented as soft-decision.

### 9.4 §1.6 gains a second column: what a block adapts, and whether that kernel is in the build

§1.6's rule, that blocks are adapters over existing kernels, stands. From here on, a row states **what it adapts** *and* **whether that kernel is linked into the workspace today**. That keeps a "designated" kernel (ADR-0010 names liquid-dsp) from being priced as a present one: T-554 found that `grep liquid crates/*/Cargo.toml` is empty (docs/18 §7.1). A block whose kernel reads **not linked** carries the cost of the binding in its own estimate, until T-607 lands or refuses it.

| Block | Adapts | In the build? |
|---|---|---|
| every §1.5 and §8.4 block | hk-dsp, hk-demod, hk-estimate kernels (§1.6 list) | **yes**, in-repo |
| `psk_demod` | liquid-dsp `symtrack_cccf` (AGC, RRC matched filter + timing, equaliser, NCO/PLL) for BPSK/DBPSK/QPSK/DQPSK/π/4-DQPSK/8PSK/D8PSK; native for OQPSK (no liquid modem) and for carrier acquisition (M-th-power spectral line) | **yes**: `hk-liquid-sys` (T-607); the native parts are in `psk.rs` (T-609) |
| `viterbi`, `viterbi_frames` | **nothing: native** (`fec/trellis.rs`). T-607 measured liquid-dsp's conv codecs to be `libfec` wrappers that are absent without `libfec` (docs/18 §7.1.1), so there was no kernel to adapt | **yes**, in-repo (T-610) |
| `reed_solomon` | liquid-dsp Reed–Solomon | **not linked** (T-607) |
| `equalise` | liquid-dsp `eqlms`/`eqrls` | **not linked** (T-607) |
| `descramble` | hk-estimate `framing::whitening` (PN9 serial/CC1101, the 7-bit LFSR both ways), generalised additively | **yes**, in-repo |
| `css_demod`, `ofdm_demod` | hk-dsp's rustfft; de-chirp, de-map and channel estimation are new DSP | **FFT yes**; the rest is new code |
| `ssb_demod`, `cw_demod` | hk-demod `audio` (Listen's `usb`/`lsb`/`cw` path); carrier estimate, raster snap and clarifier are new DSP (§8.4) | **partly** |
| `mlevel_slicer`, `bitstuff`, `codeword_map`, `despread` | new code; no kernel needed | **n/a** |

**Unverified; T-607 must check it before pricing T-610 and T-611.** liquid-dsp's convolutional and Reed–Solomon codecs are reported to be available only when Phil Karn's `libfec` is present when liquid-dsp is built. If so, linking liquid-dsp alone does not deliver them, and `libfec` is a second C dependency with its own licence to read.

### 9.5 A `raster` output kind: not taken, and the rules if it is

It is not taken here. No ticket funds it (docs/18 §9: rank 13, one row of families), and it is not needed to answer §9.2. If a later ticket takes it (HF fax, SSTV, APT), it binds to §8's pattern:
- a `raster` `outputs[]` kind beside `audio`, fed by a sink block;
- the same **double gating**: content at egress, plus the `content_class` ceiling and `output_policy` clamp at pipeline start. Declaring a raster output together with a class that forbids content is a validation error;
- the same **schema-version rule**: it joins §8.6's version 3 if it lands before version 3 is released (the code is still at 2), and bumps to 4 otherwise.

Its wire form is that ticket's question. §4's reasoning applies: a new stream `kind` is a **major** stream-contract change, so reusing an existing kind comes first.

### 9.6 What does not change

- No port type is added (§1.1 is unchanged), and `schema_version` is not bumped. New blocks are catalogue data: a recipe naming one validates against the catalogue, and a placeholder only warns.
- The M1 catalogue and the §8 audio rows are untouched. ADR-0015 §10's M-14 ("optional `psk_demod`") is superseded by T-609, as filed.
- **T-609 landed `psk_demod` (2026-09-23).** Its parameters are now pinned (`params_pinned: true`), and the §9.1 row above lists them. It keeps §9.2's contract exactly: k soft items per symbol, MSB first, max-log magnitude. Hot `rotation_deg` / `iq_swap` resolve the phase ambiguity; for OQPSK, `rotation_deg` selects inverted rails, because a 90° slip there is a one-bit slip plus a rail inversion. `Recipe::validate` now refuses any diagnostic output wired into a node input, as §9.2 required before registration. `crates/hk-blocks/src/blocks/iq/psk.rs` documents the acquisition, hold and DISCONTINUITY behaviour, and the measured reasons carrier acquisition is a feed-forward M-th-power line search rather than the "FLL or band-edge" the ticket sketched.
- **T-610 landed `viterbi` and `viterbi_frames` (2026-09-23)** on one native trellis engine (`crates/hk-blocks/src/blocks/fec/{trellis,viterbi}.rs`; the module docs are normative for the code conventions). Parameters pinned as in §9.1; `puncture` became strings (`"101"`) because a hex mask cannot state its period. The streaming shape decides every bit with **at least `traceback_bits`** of trellis after it; soft input is unquantised correlation, and `bits` input is hard decision **reported as status `hard_decision = 1`** (both shapes; `viterbi_frames` always, per §9.3). `align: auto` scores one decoder per phase by path-metric growth and switches **continuously in time** (the new phase resumes at the step holding the old phase's first undecided item; not a `DISCONTINUITY`, counted in `realignments`). `error_rate` is the channel BER estimated by re-encoding the decided path — a refinement objective. Measured (`viterbi_tests.rs`): CCSDS R = 1/2 K = 7 soft BER 3.7e-4 at 3 dB and 6.7e-5 at 3.5 dB (0.73× and 0.67× the union bound); hard decision ≈ 2 dB worse (hard at 5 dB ≈ soft at 3 dB); the punctured CCSDS rates 2/3–7/8 decode from any phase; a BPSK IQ → `psk_demod` → `viterbi` → ASM chain recovers SIGNAL-034 frames blind. CCSDS 131.0-B has no convolutional test vector; the known-vector test is the standard's generator figure and ASM, cross-checked against the published Meteor-M LRPT correlator word.
- **A catalogue gap is a product-visible state** (ADR-0015 §8, T-550). "This build has no `css_demod`" must never read like "this is not LoRa". `hk_pipeline::synth` already says so for `psk_demod`, and each new row inherits the obligation until its block registers.
