# Tutorial 1: RDS from blocks

This is the decoder workbench's reference tutorial (M1, T-094; [docs/13](../13-m1-decoder-workbench.md), [ADR-0011](../adr/0011-decoder-workbench-contracts.md)). It decodes RDS, the 1187.5 Bd data channel on the 57 kHz subcarrier of broadcast FM, using only built-in blocks wired together by a recipe: [`recipes/rds.recipe.json`](../../recipes/rds.recipe.json).

It then runs, hot-edits and saves that recipe on a live station through the API. Every number below comes from the project's FM capture `fm_100p8M_2p4M_l32g30a1_t1p5_5s` (HackRF One, 101.3 MHz, 5 s) replayed by the mock SDR. The station is found blind; nothing tunes to a looked-up frequency.

## 1. The chain, stage by stage

A recipe is a block graph (ADR-0011 §2). `POST /api/recipes/validate` checks every node against `GET /api/blocks` and answers the typed edges:

```text
input(iq, 240 kS/s) → fm → rds57 → clock → slice → diff → sync → crc → group ─┬→ ps
                      real   iq     soft    bits    bits  frames frames frames └→ rt
```

The input stage is a channel DDC. It centres the target and resamples it to `input.sample_rate_hz` (240 kS/s, 200 kHz wide). Each stage publishes a status readout: flat `<node>.<metric>` keys in `GET /api/pipelines/{id}` `status`, and a `status` record on the inspector stream about every 250 ms.

The values in the table are from the walkthrough in §2, 20 s after start.

| Node | Block | What it does | Readout on the capture |
|---|---|---|---|
| `fm` | `fm_demod` (`deviation_hz` 75 000) | Quadrature discriminator: IQ → the FM multiplex (MPX), a real signal. | `offset_hz` −1842 (carrier offset from the channel centre, which the discriminator removes as DC) |
| `rds57` | `subcarrier` (57 kHz, 4.8 kHz wide, 9.5 kS/s out, `reference` 3 × the 19 kHz pilot, `phase_tracking: bpsk`) | Locks a PLL to the pilot, mixes the MPX down by 3 × pilot and filters. BPSK phase tracking removes the residual phase. | `lock` locked, `pilot_locked` 1, `quality` 0.57 |
| `clock` | `clock_recovery` (1187.5 Bd, `pulse: biphase`, `algorithm: max-contrast`) | Biphase (Manchester) symbol timing: picks the half-symbol split with the largest contrast and emits one soft symbol per bit. | `lock` locked, `timing_contrast` 1.64, `snr_db` 7.6, `quality` 0.85 |
| `slice` | `slicer` (threshold 0) | Soft symbols → hard bits. | `ones_fraction` 0.50 |
| `diff` | `diff_decode` (`xor`) | RDS is differentially encoded: `b[k] = d[k] ⊕ d[k−1]`. | — |
| `sync` | `sync_search` (`offset-words`, 26-bit blocks, poly 0x5B9, offsets A B C/C′ D) | Finds block boundaries from the syndromes of the offset words, locks after 2 blocks in sequence, then cuts 104-bit groups. | `lock` locked, `blocks_ok` 821, `blocks_bad` 74 (block error rate 0.06 over 20 s, including the loop seams), `acquisitions` 5 |
| `crc` | `crc` (width 10, poly 0x5B9, per-block offsets, `strip`) | Checks each block's check word and strips it, leaving a 64-bit group of four 16-bit words. Each frame is marked `valid` or `invalid`. No burst correction: RDS's false-correction bound rules it out (T-087). | `frames_ok` 166, `frames_bad` 56 (`error_rate` 0.10 per block, `quality` 0.90) |
| `group` | `fields` (`map: rds_group`) | The recipe's field map lays out PI, group type, version, TP and PTY, plus layers conditional on the group type: 0A/0B PS segment and 2A/2B RadioText segment. Each frame record carries the parsed layer tree (stream contract §14.2). | `frames_ok` 222, `frames_partial` 0 |
| `ps` | `text` (4 segments of 2 characters, keyed by PI) | Assembles the 8-character programme service name. | `strings` 6 |
| `rt` | `text` (16 × 4 characters, reset on the A/B flag, terminator 0x0D) | Assembles RadioText. | `quality` 0.94 (15 of 16 segments seen), `strings` 0 (see §3) |

**Recipe parameters.** The worked example needed no changes to decode real RDS. Parameters were compared side by side as parallel pipelines on the same station over 30 s:
- subcarrier bandwidth 3.2 / 4.8 / 6 kHz;
- `max-contrast` vs `gardner` timing;
- clock `loop_bandwidth` 0.003 / 0.01 / 0.03.

Every variant gave 76–78% CRC-valid groups and 8% block errors. That is the capture's own limit: the reference decoder measured 8.7% BLER on the full recording, T-023.

## 2. Run it on a station (API)

Start `hk serve` on the recording through the mock SDR (or on the radio with `--hackrf`), with a token:

```sh
HK_TOKEN=... hk serve --device mock:fixtures/hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta
```

Every `curl` below sends `-H "Authorization: Bearer $HK_TOKEN"`. The full route table is in [docs/api.md](../api.md) "Recipes and pipelines".

1. **Find the station.** Take an emitter from `GET /api/inventory`, which the blind detector fills. On the capture the station is found at 101.3017 MHz, 225 kHz wide.
2. **Start the recipe on it:**
   ```sh
   curl -X POST $API/api/pipelines -d '{"recipe_id":"rds","target":{"emitter_id":"<id>"}}'
   ```
   The answer is `201`. The pipeline has `channel {center_hz: 101301659.5, bandwidth_hz: 200000, sample_rate_hz: 240000}` and `content_class: unrestricted` (the FM band prior). Its outputs are `inspector/p9/groups`, `stage/p9/mpx` and `stage/p9/symbols`.
3. **Read frames.** Either open the WebSocket `/ws/inspector/<p>/groups`, or connect over TCP and send the line `inspector/<p>/groups?token=…`. One NDJSON frame record arrives per group:
   ```json
   {"type":"frame","crc_status":"valid","decoder":"recipe:rds@1","metadata":{"sample_index":…,"bit_len":64,"edit_rev":0,"fit":"ok"},
    "content":{"hex":"1694…","layers":{"nodes":[{"path":"pi","value":5780,"text":"0x1694"},{"path":"group_type","value":0},…,{"path":"ps.chars","value":"Un"}]}}}
   ```
   Assembled strings come from an on-demand stage tap on a text node, `open/stage?pipeline=<p>&node=ps`. On the capture that gives `ps.text = "Unstoppa"`, PI 0x1694, PTY 7 and TP false.
4. **Watch the stages.** Poll `GET /api/pipelines/<p>` for the status in the §1 table and for `stats`. Over the 20 s window stats read 48 021 504 samples, 223 frames and `gaps` 0.

## 3. Hot-edit it

RadioText is 16 segments. With `emit: on-complete` a string is emitted only once every segment has arrived CRC-valid since the last emission. This 5 s capture loops, and the same segment fails CRC on every pass, so on-complete never emits. That is what `rt.quality` 0.94 / `rt.strings` 0 says: 15 of 16 segments seen, nothing emitted.

`emit` is a hot parameter, so switch it without stopping anything. Send the whole draft recipe with the one change:

```sh
curl -X PUT $API/api/pipelines/p9/recipe -d @rds-onchange.json   # rt.params.emit = "on-change"
```

The answer is `200`:
- `edit_rev` 1, `applied_at_sample` 1906294784;
- plan: `rt` `params-hot`, every other node `unchanged`;
- swap: `{kept: 9, updated: 1, rebuilt: 0, reset: 0}`.

The swap happens at the next chunk boundary. Capture never pauses and no sample is lost. An `edit` record marks the boundary on the inspector stream, and later frames carry `edit_rev: 1`.

Six seconds later `rt.strings` is 15. The strings fill in segment by segment, and the per-segment consensus is `Star 101.3 - Quit Playing Games - Backstreet    s`. The one segment that never arrives CRC-valid in the loop stays as spaces.

Then save the running revision as the recipe's next version:

```sh
curl -X POST $API/api/pipelines/p9/save      # 201 {"id":"rds","version":2,"pipeline_id":"p9"}
curl -X DELETE $API/api/pipelines/p9         # {"stopped": {"state":"ended","end_reason":"stopped",…}}
```

Saved versions live in `<data dir>/recipes/rds/<version>.json`, and `GET /api/recipes/rds` now answers version 2. Built-in recipes are read-only.

## 4. How it is tested

`tests/e2e/tests/acceptance/tutorial_rds.rs` is part of the `acceptance_m0` suite. Both tests follow the same path as §2: the recording loops through the mock SDR with its ground truth stripped, the station is matched blind from `/api/inventory`, the pipeline is started through the API, and frames are read over TCP.

- **Real capture vs truth and oracle.**
  - PI, PTY and TP must equal the hidden truth, and every assembled PS frame must be in the truth's PS set.
  - The existing Rust decoder (`hk_demod::rds`, the oracle) runs on the same recording at the same blindly found centre. It must agree on PI, PTY and the most frequent PS.
  - **Group-level agreement:** of the groups the oracle decoded with all four blocks valid, the share the recipe also decoded CRC-valid with identical fields (PI, group type and version, TP, PTY, PS segment and characters), at the same recording position within ±4 bits. This must be at least 0.9, with no conflicting group.
    - The oracle counts bit positions from its pilot lock, because `WfmDemod` feeds RDS only after the pilot PLL locks. The test therefore estimates that constant lag first: the most common position difference between identical groups.
  - Then the hot edit and save of §3 are asserted, with RadioText segments consistent across strings.
- **Synthetic, exact.** The `fm_broadcast_rds` scene, with RadioText added in T-094, is checked for exact PI, PTY, PS (`HACKRIFF`) and RadioText (`HACKRIFF TUTORIAL 1 - RDS BUILT FROM BLOCKS`).
  - Its group lattice is exact, so the test also asserts that every CRC-valid frame's `metadata.sample_index` is its group's first bit (±2 bits) and carries that slot's content.

Results of the last run:

| | Real capture (2 passes of the 5 s loop) | Synthetic |
|---|---|---|
| Station found blind | 101.3022 MHz, 230 kHz | 99.5000 MHz |
| PI / PTY / TP | 0x1694 / 7 / false (truth: 1694 / 7 / false) | 0xC0DE / 10 (exact) |
| PS | `Unstoppa` × 4 (oracle `Unstoppa`) | `HACKRIFF` |
| RadioText | on-change consensus `Star 101.3 - Quit Playing Games - Backstreet    s` (one segment never CRC-valid in the loop) | `HACKRIFF TUTORIAL 1 - RDS BUILT FROM BLOCKS` |
| CRC-valid groups | 86 / 110 = 0.78 (oracle: 44 / 55 groups, BLER 0.059) | 95 / 95 = 1.00 |
| Group agreement with the oracle | 44 / 44 = 1.00, 0 conflicts (oracle position lag 122 bits = 102.7 ms, its pilot-lock time) | first-bit lattice 98 / 98 |

Both tests take about 42 s together.

```sh
HK_E2E_REQUIRE_SYNTH=1 HK_REQUIRE_FIXTURES=1 cargo test -p hk-e2e --test acceptance_m0 tutorial_rds -- --nocapture
```
