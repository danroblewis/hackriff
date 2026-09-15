# Tutorial 2: POCSAG from blocks (T-095, T-109)

This is the decoder workbench's second tutorial (M1, T-095; [docs/13](../13-m1-decoder-workbench.md), [ADR-0011](../adr/0011-decoder-workbench-contracts.md)). It decodes POCSAG (ITU-R M.584 "Radio paging code no. 1") pager messages using built-in blocks wired by a recipe: [`recipes/pocsag.recipe.json`](../../recipes/pocsag.recipe.json), with `follow_hops` following every channel of a multi-channel pager net at once.

**Status: passing, blind** (`tests/e2e/tests/acceptance/tutorial_pocsag.rs`, measured 2026-09-15). The mock SDR replays the scene in a loop with its truth stripped. `/api/inventory` finds all four channels. `follow_hops` takes its channel set from those blind detections. Every page matches the hidden truth, and the simulcast page comes out once.

## 1. The chain, stage by stage

```text
input(iq, 24 kS/s per channel, follow-hops) → fsk → clock → slice → sync → bch → msg ─┐
                                                                                        ├→ hops → page
                                                                     (once per channel) ─┘
```

- `fsk` (`fsk_demod`): non-coherent 2-FSK discriminator. It has no `deviation_hz` param; it estimates deviation per channel from the running RMS frequency (measured 4.7 kHz against the 4.5 kHz truth).
- `clock` (`clock_recovery`, NRZ, Gardner, 1200 Bd): one soft symbol per bit.
- `slice` (`slicer`, inverted): the higher frequency (mark) becomes bit 0, matching `gen_pocsag.c`'s convention (`hkpy.synth.pocsag`, "bit 1 = −deviation").
- `sync` (`sync_search`, `sync-word` mode, `0x7CD215D8`, ≤2 bit errors, 512-bit frames): finds batch boundaries and cuts 16 codewords per batch.
- `bch` (`bch`, BCH(31,21) + even parity, `correct_bits: 2`): checks each codeword and corrects it in place.
- `msg` (`assemble`): an address codeword opens a message (the RIC's low 3 bits select the frame slot), later codewords append payload, and an idle codeword or the next address closes it.
- `hops` (`follow_hops`): merges every channel's assembled messages in source-time order and deduplicates them. The same bytes from a different channel within `dedupe_s` (1 s) count as a simulcast copy, not a new page.
- `page` (`fields`, `pocsag_message` map): RIC (`address ‖ slot`), function, and numeric (4-bit BCD) or alphanumeric (7-bit, LSB-first) text.

**Legal guardrail (ADR-0004).** The shipped recipe's `output_policy.content_class` is `restricted-paging`: its outputs carry metadata only (no RIC or text). Real paging traffic stays metadata-only. The tutorial's synthetic scene isn't intercepted traffic, so the test first checks that default. It then starts a **content-vouched draft** of the recipe (`output_policy.content_class: "unrestricted"`, plus `hackriff:content_class` vouched on the recording) so it can assert on the decode. The built-in recipe is never edited.

## 2. The synthetic scene

`hkpy.synth.pocsag_pagers` (T-098) generates a multi-channel POCSAG net. The tutorial asks for four channels on a 25 kHz raster around 152.360 MHz, all at 1200 Bd:

| Channel | Offset | Key-up (after `start_s`) | RIC | Function | Message |
|---|---|---|---|---|---|
| 0 | −50 kHz | 0 s | 1234560 | numeric | `911234` |
| 1 | −25 kHz | 0.21 s | 1876544 | alphanumeric | `STANDBY AT GATE 12` |
| 2 | +25 kHz | 0.37 s | 654320 | alphanumeric | `HACKRIFF PAGE TEST` |
| 3 | +50 kHz | 0.21 s | 1876544 | alphanumeric | `STANDBY AT GATE 12` (simulcast of channel 1) |

Each RIC's frame slot is 0, so every message fits one 16-codeword batch (about 0.93 s including the 576-bit preamble).

T-109 changed the scene in two ways:
- **Independent key-ups.** A new `start_offsets_s` param (missing entries default to 0, so the old behaviour stays the default). Independent channels key at their own times; the simulcast pair keys together, as a real simulcast net does. T-095 keyed all four channels in lockstep, which no real net does.
- **A 25 kHz raster.** T-095 used offsets of ±55/18/20/57 kHz. The recipe DDCs a 16 kHz channel, and a target must sit inside ±0.49·fs of the tuned centre. At +57 kHz the channel's upper edge reached +65.1 kHz, past the +64.8 kHz limit, so the start was refused with `outside_window`. The test's `band` is now the usable window (±0.49·fs), still never a channel list.

## 3. What blocked T-095, and the fix (T-109)

T-095 found only 1 of 4 channels blind: the inventory held one 118 kHz-wide row and a 49 kHz-wide row spanning several channels. T-109 measured each stage over the scene looped 8 times (`hk replay`):

| Stage | Before T-109 | After |
|---|---|---|
| Detections | 32 boxes, 12.1 kHz each, one per channel per loop | same |
| Channel tracks | 4, one per channel, 8 detections each | same |
| Hop sets | **1**: all four channel tracks linked, 32 detections, 124 kHz wide | none |
| Inventory | **1 wide hop-set row** (−58…+63 kHz, 118 kHz); channel tracks aren't offered because they're hop-set members | 4 channel rows (12 kHz each), plus narrow spurious rows |

Detection and tracking were correct; the channels merged in hop-set linking, in two defects:

1. **False hop set** (`crates/hk-detect/src/track/tracker.rs`, `Tracker::hop_check`). The contiguous hop rule links a finished burst to a burst on another channel that ended just before it started. With the recording looping, each loop's burst on one channel abuts the next loop's burst on another. After 10 links across 3 channels, a "hop set" formed, and the inventory folded all four channels into one wide row. A frequency hopper is on one channel at a time. The bursty-hopper path (`nearest_similar`) already vetoed concurrent bursts; the contiguous path didn't. **Fix:** a concurrency veto (new helper `Tracker::keyed_during`). The link is refused when the successor's track had another burst overlapping the predecessor in time, or the predecessor's track had one overlapping the successor. The veto counts in `TrackerStats::hop_concurrent_vetoes`. The rule is blind, with no raster and no lookups. Unit test: `close_packed_co_keyed_channels_stay_separate_emitters_not_a_hop_set` (`crates/hk-detect/tests/track.rs`). Before the fix, that test showed 24 hop links and 1 hop set; after it, 0 of each.
2. **Channels that never idle never reach the inventory live** (`crates/hk-pipeline/src/detect.rs` `DetectNode::flush`, `Writer`; `crates/hk-pipeline/src/inventory.rs`). Only closed tracks and hop sets became inventory sightings, and a track closes after 60 s idle. A pager net looping every second never idles, so live serve found 0/4 once the false hop set was gone. **Fix:** every 5 s of stream time, `DetectNode::flush` offers the open, confirmed channel tracks with at least 4 bursts (`Tracker::summaries`) through the new `Inventory::live_track`. The sighting is keyed by the track id, so a re-offer and the final close add only new bursts. Hop-set members, in-band fragments and merged tracks stay excluded (`track_sighting`). There's no auto-confirmation on a partial life; the close re-offers with the full evidence. Continuous single-burst carriers (FM stations) still enter at close, unchanged. Unit test: `inventory::tests::t109_live_channel_track_enters_the_inventory_before_close_without_double_counting`.

## 4. Results (measured 2026-09-15)

`HK_E2E_REQUIRE_SYNTH=1 cargo test -p hk-e2e --test acceptance_m0 -- tutorial_pocsag --nocapture`:

- **Channel discovery:** all 4 channels found blind, in 4 inventory rows. `follow_hops` resolved 4 lanes from `detections`: 152.310090, 152.335130, 152.385057 and 152.410140 MHz (within 140 Hz of truth).
- **Pages:** in the final passing run, 16 frame records came out, **all 16 CRC-valid**. The count varies by a loop between runs (an earlier run gave 18/18), depending on how many loops fall in the test's wait. Every page matches truth on RIC, function and text, tagged with its own channel's lane: channel 0 (`911234`) ×6 on lane 0, channel 2 (`HACKRIFF PAGE TEST`) ×5 on lane 2, and the simulcast (`STANDBY AT GATE 12`) ×5 on lane 1, once per loop, never twice for one transmission.
- **Dedupe:** `hops.dups` 5, one per loop for the simulcast pair (in its status snapshot, `hops.items_in` 20 and `items_out` 15); `hops.late` 0.
- **BCH:** 80 codewords per lane (320 total). All passed without correction: 0 corrected, 0 bad, 0 corrected bits (the scene is at 22 dB SNR). The test asserts at most 5 % bad words.
- **Oracle:** `multimon-ng` (POCSAG1200) run on the tutorial's own IQ, the whole recording, one channel at a time (`py/tests/test_synth.py::test_multimon_ng_decodes_the_tutorial_pager_net_per_channel`), decodes exactly one page per channel, matching truth: 4/4.

## 5. Limits

- **One baud per recipe.** `follow_hops` builds every channel's upstream graph from the same recipe and params (`crates/hk-pipeline/src/recipes/hops.rs` `build_lane`), so the recipe pins 1200 Bd for every lane. A net mixing 512, 1200 and 2400 Bd channels needs one pipeline per baud. The encoder is validated at all three bauds independently (`test_pocsag_pages_demodulate_and_decode`, `test_multimon_ng_decodes_pocsag_pages`).
- **Multi-batch pages.** Every tutorial page fits one batch, so frames longer than `order_window_s` aren't exercised here. T-107's end-ordered merge covers them; `hops.late` stays 0 in this run.
- **Clock lock status.** The `clock` stage reports `lock: searching` (quality about 0.64) on channels 1–3 even though every one of their codewords decodes. The status is likely a snapshot taken between that channel's staggered bursts. It's a cosmetic status issue, not a decode issue; not investigated.
- **Live inventory latency.** A repeating channel appears after 4 bursts and at the next 5 s offer. A slower net (one page a minute) shows up after about 4 minutes, or when its track closes.
