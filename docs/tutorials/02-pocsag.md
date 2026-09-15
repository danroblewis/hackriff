# Tutorial 2: POCSAG from blocks (blocked, T-095)

This is the decoder workbench's second tutorial (M1, T-095; [docs/13](../13-m1-decoder-workbench.md), [ADR-0011](../adr/0011-decoder-workbench-contracts.md)). It decodes POCSAG (ITU-R M.584 "Radio paging code no. 1") pager messages using built-in blocks wired by a recipe: [`recipes/pocsag.recipe.json`](../../recipes/pocsag.recipe.json), with `follow_hops` following every channel of a multi-channel pager net at once.

**Status: blocked.** The recipe and the acceptance test (`tests/e2e/tests/acceptance/tutorial_pocsag.rs`) are written and the code compiles and lints clean, but the test is `#[ignore]`d: blind detection does not yet resolve the synthetic pager net's four simultaneous channels into four separate inventory emitters (measured 2026-09-15, below). The decode-vs-truth assertions were never exercised. `docs/tasks.yaml` T-095 carries the finding as a follow-up.

## 1. The chain, stage by stage

```text
input(iq, 24 kS/s per channel, follow-hops) → fsk → clock → slice → sync → bch → msg ─┐
                                                                                        ├→ hops → page
                                                                     (once per channel) ─┘
```

- `fsk` (`fsk_demod`): non-coherent 2-FSK discriminator. No `deviation_hz` param: it self-estimates deviation from the running RMS frequency, per channel.
- `clock` (`clock_recovery`, NRZ, Gardner, 1200 Bd): one soft symbol per bit. The recipe currently pins one baud (1200) for every lane, since `follow_hops` builds every channel's upstream graph from the same recipe/params (`crates/hk-pipeline/src/recipes/hops.rs` `build_lane`) — true per-channel baud (512/1200/2400 at once) isn't supported by the current architecture. `py/tests/test_synth.py::test_pocsag_pages_demodulate_and_decode` and its `multimon-ng` oracle already validate the encoder/framing at all three bauds independently of this recipe.
- `slice` (`slicer`, inverted): higher frequency (mark) → bit 0, matching `gen_pocsag.c`'s convention (`hkpy.synth.pocsag`, "bit 1 = −deviation").
- `sync` (`sync_search`, `sync-word` mode, `0x7CD215D8`, ≤2 bit errors, 512-bit frames): finds batch boundaries and cuts 16 codewords.
- `bch` (`bch`, BCH(31,21) + even parity, `correct_bits: 2`): checks and corrects each codeword in place.
- `msg` (`assemble`): address codeword opens a message (RIC's low 3 bits select the frame slot), later codewords append payload, an idle codeword or the next address closes it.
- `hops` (`follow_hops`): merges every channel's assembled messages, ordered by source time and deduplicated (same bytes from a different channel within `dedupe_s` is a simulcast copy, not a new page).
- `page` (`fields`, `pocsag_message` map): RIC (`address ‖ slot`), function, and numeric (4-bit BCD) or alphanumeric (7-bit, LSB-first) text, per `crates/hk-recipe/src/fields/mod.rs`'s POCSAG value-key example.

**Legal guardrail (ADR-0004).** The shipped recipe's `output_policy.content_class` is `restricted-paging`: by design, its outputs carry metadata only (no RIC/text) unless the recipe's own declared class is loosened — real paging traffic stays metadata-only. The tutorial's own synthetic scene is not intercepted traffic, so its acceptance test starts a **content-vouched draft** of the recipe (`output_policy.content_class: "unrestricted"`, plus `hackriff:content_class` vouched on the recording) purely to exercise and assert on the decode chain; the saved built-in recipe is never edited.

## 2. The synthetic scene

`hkpy.synth.pocsag_pagers` (T-098) generates a multi-channel POCSAG net. The T-095 test asks for four channels at 152.360 MHz ± {−55, −18, +20, +57} kHz, all 1200 Bd:

| Channel | Offset | RIC | Function | Message |
|---|---|---|---|---|
| 0 | −55 kHz | 1234560 | numeric | `911234` |
| 1 | −18 kHz | 1876544 | alphanumeric | `STANDBY AT GATE 12` |
| 2 | +20 kHz | 654320 | alphanumeric | `HACKRIFF PAGE TEST` |
| 3 | +57 kHz | 1876544 | alphanumeric | `STANDBY AT GATE 12` (simulcast of channel 1) |

Each channel's RIC is chosen so its address's frame slot is 0, keeping every message inside a single 16-codeword batch (~0.93 s of audio plus the 576-bit preamble). All four channels key up at the recording's same `start_s`, as a real simulcast pager net would.

## 3. What is blocked, and the measurement

The test drives the scene blind through the mock SDR (`crate::blind::blind_live_streams`) exactly like [Tutorial 1](01-rds.md), then polls `/api/inventory` for a row matching each of the four channels' truth extent before ever starting the recipe (never a hard-coded frequency list — `follow_hops` is meant to pick its channel set from this same blind `detections` source, `hops.rs::resolve_channels`).

Measured 2026-09-15: only 1 of 4 channels resolved to its own inventory row. The inventory held three rows:

| Centre | Bandwidth |
|---|---|
| 152.41787 MHz | 0.78 kHz |
| 152.36118 MHz | 118.14 kHz |
| 152.39857 MHz | 49.04 kHz |

The narrow row (152.41787 MHz) is close to channel 3's expected 152.417 MHz but far too narrow to be its ~10.2 kHz burst. The two wide rows straddle multiple channels each (152.36118 MHz spans roughly channels 0–2's range at 118 kHz wide; 152.39857 MHz spans channels 1–2 at 49 kHz). Four simultaneous, closely spaced (35–38 kHz apart) narrowband bursts starting at the same instant are being clustered into a small number of wide detections rather than resolved per channel.

Root cause not isolated (out of scope for this pass): candidates are the detection/clustering or hop-set linker treating co-timed bursts across nearby channels as one wideband event, or the scene needing staggered `start_s` per channel (real pager batches are not perfectly co-timed at the sample level either). Whichever it is, `follow_hops` never gets a usable multi-channel set, so the run never reaches decoding, and the RIC/function/text-vs-truth assertions, the `follow_hops` dedupe check and the `multimon-ng` oracle comparison in the test were never exercised.

The test is `tests/e2e/tests/acceptance/tutorial_pocsag.rs`, `#[ignore]`d with this finding; `docs/tasks.yaml` T-095 is `blocked` with the same note for the follow-up. Run it manually once the blocker is fixed:

```sh
cargo test -p hk-e2e --test acceptance_m0 tutorial_pocsag -- --ignored --nocapture
```

## 4. What is verified

- The recipe (`recipes/pocsag.recipe.json`) validates against the block registry and splits correctly at its `follow_hops` node (`crates/hk-pipeline/src/recipes/hops.rs` test `the_pocsag_tutorial_recipe_splits_at_its_merge_node`, from T-093).
- The `pocsag_message` field map's bit layout (RIC = address ‖ slot with `skip_bits` over the function bits, numeric BCD, alphanumeric 7-bit LSB-first) matches a validated example in `crates/hk-recipe/src/fields/mod.rs`.
- `hkpy.synth.pocsag`'s encoder is independently cross-checked against the public-domain `gen_pocsag.c`/`bch.c` reference (`py/tests/test_synth.py::test_pocsag_bch_matches_published_sync_and_idle_codewords`) and, where `multimon-ng` is installed, against `multimon-ng` itself (`test_multimon_ng_decodes_pocsag_pages`, 3/3 channels at 512/1200/2400 Bd).
- The full Rust test file compiles and passes `cargo clippy --workspace --all-targets -- -D warnings`.

What remains: diagnose why blind detection doesn't separate this scene's four channels (or adjust the scene), then re-run the ignored test and fill in real decode/BCH/dedupe/oracle numbers here.
