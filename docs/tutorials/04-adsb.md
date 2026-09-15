# Tutorial 4: ADS-B from blocks

M1, T-097; [docs/13](../13-m1-decoder-workbench.md), [ADR-0011](../adr/0011-decoder-workbench-contracts.md). Decodes 1090 MHz Mode S extended squitters (ADS-B DF17) using built-in blocks wired by a recipe: [`recipes/adsb.recipe.json`](../../recipes/adsb.recipe.json).

Unlike [Tutorial 1 (RDS)](01-rds.md), the target here is not found through `/api/inventory`: ADS-B squitters are ~120 µs bursts, and burst detections never cluster into a stable inventory emitter on their own (see §2). So this tutorial finds the band from the burst detector's own blind measurements directly, then attaches the recipe to that band. Everything below replays the synthetic `adsb_squitter` scenario (4 aircraft × squitters, 2.4 Msps at 1090 MHz); a real 1090 MHz capture is blocked on the antenna (T-025), same as `SIGNAL-001`.

## 1. The chain, stage by stage

```text
input(iq, 2.4 MS/s, 2 MHz) → ppm → crc → msg(fields) → frames (inspector)
```

| Node | Block | What it does |
|---|---|---|
| `ppm` | `ppm_demod` (`bit_rate_bd` 1 000 000, `chips_per_bit` 2, preamble `0xA140` over 16 chips, `min_snr_db` 9, `length_from`: DF (bits 0–4) 16–31 → 112 bits, else 56) | Scans for the 8 µs preamble (pulses at chips 0, 2, 7, 9), then decodes each data bit as early-chip-vs-late-chip energy (PPM: bit 1 = pulse-then-quiet, bit 0 = quiet-then-pulse). Chips are read as interpolated magnitudes at their centres from a *fractional* frame origin: per detected preamble it decodes the frame over a 1/8-chip phase grid and keeps the phase with the largest decision margin whose preamble still passes (T-110; detection itself also tries the half-sample position). The frame ends at 56 or 112 bits depending on the downlink format it reads as soon as those bits arrive. |
| `crc` | `crc` (width 24, poly `0xFFF409`, `span: {start_bit: 0, end_trim_bits: 0}`, `strip: false`) | Checks the whole frame's CRC-24 (Mode S parity, MSB-first, no reflection). No burst correction (`correct_burst_bits` left unset): matches readsb's own `--no-fix` choice — see §4. DF 0/4/5/20/21 overlay the ICAO address on the parity field, so those frames' `crc_status` fails by design; this fixture and recipe only exercise DF17. |
| `msg` | `fields` (`map: adsb_frame`) | DF, `ca`; ICAO (DF 11/17/18); for DF17/18, the ME field: `tc` (type code) then, conditioned on `tc`, identification (category + raw 48-bit callsign bytes), airborne position (surveillance status, altitude — AC12 with its Q-bit `skip_bits` out, `scale: 25, add: -1000` → ft — and the two raw 17-bit CPR ints), or airborne velocity (`vew`/`vns` magnitudes plus `dew`/`dns`/`svr` direction enums, and vertical rate `scale: 64, add: -64`). |

The `adsb_frame` field map's bit layout was checked bit-for-bit against `py/hkpy/synth/adsb.py`'s encoder (`me_identification`, `encode_altitude`, `me_airborne_position`, `me_velocity`) while tuning this recipe — every offset, `skip_bits` position and `scale`/`add` lines up exactly, which is why every CRC-valid frame's fields matched the hidden truth exactly in every run below (0 mismatches, always).

**Recipe parameters tuned in T-097** (the skeleton's placeholders didn't decode at all):
- `input.bandwidth_hz` was 2 000 000, equal to `input.sample_rate_hz` — the DDC's own planner rejects a channel with **zero** transition-band headroom (`bandwidth/2 < stopband ≤ rate − bandwidth/2` fails at equality), so every attempt to start the recipe returned `422 unrealisable`. T-097 lowered it to 1 600 000 Hz; **T-110 moved the input to 2 400 000 samples/s with a 2 000 000 Hz channel**, because at 2 Msps the 1.6 MHz channel cuts the chip-rate content (measured below: 12/16 at 2.0/1.6 even with the fixed `ppm_demod`, 16/16 at 2.4/2.0).
- `ppm.min_snr_db` was left at the block's default (6 dB); raised to 9 dB, which measurably cut false preamble triggers on noise without reducing genuine recall (identical real-squitter decode counts at 6 and 9 dB across repeated runs).
- `ppm.correct_burst_bits` is deliberately **not** set. CRC-24 over a 112-bit frame allows 1-bit correction within the false-correction bound (ADR-0011 §5.1, `crates/hk-blocks/src/blocks/fec/crc.rs`), but `hk-plugin-readsb`'s own module doc records that readsb's `--fix` (its default) prints repaired-but-wrong frames whose recomputed CRC then checks out — measured there as 20/20 lines with `--fix` vs 10/10 genuinely good with `--no-fix` on the same input, which is why the wrapper always passes `--no-fix`. Recipe correction is left off for the same reason: a valid-looking wrong ICAO/position is worse than a missed message.

## 2. Finding the target blind

RDS's continuous FM carrier turns into a stable `/api/inventory` emitter from spectral peak tracking alone. ADS-B squitters don't: `hk-pipeline/src/detect.rs`'s burst path (`hk-detect/burst@…`) stores its detections but never hands them to the tracker ("untracked"), so no `Sighting`/`Emitter` is ever created from bursts alone — confirmed by `SIGNAL-001`'s burst-path test, which finds every squitter blind via raw `Detection` rows and notes "untracked rows (no emitter)". An emitter only appears once a decoder (readsb) resolves an ICAO identity.

So the tutorial (and both acceptance tests) instead:
1. Start the live run through the mock SDR as usual (`hk serve`-shaped: the composed pipeline's burst-detection reader runs regardless of any recipe).
2. Poll `Repository::detections_in_region` for `hk-detect/burst@…` rows (this is the system's own blind RF measurement — no database, no truth, no band-plan lookup) until there are enough, then take their **median** centre frequency.
3. `POST /api/pipelines {"recipe_id": "adsb", "target": {"band": {"f_lo": median-0.4MHz, "f_hi": median+0.4MHz}}}`. Only the band's centre matters: `channel_plan` always uses the recipe's own fixed `input.bandwidth_hz` for the actual channel.

On the synthetic fixture this reliably lands within tens of kHz of 1090.000 MHz, comfortably inside the FM/ADS-B band-plan prior's `unrestricted` window and the mock's tuned Nyquist.

**Decode rows (T-111; the gap T-097 found).** The recipe's `aircraft` output (`kind: "messages"`) ingests decodes into the Repository the way readsb's own decodes do, so the recipe gets its own `/api/inventory` emitters once it decodes ICAOs. Each CRC-valid squitter with an `icao` becomes a `recipe:adsb` Decode row with identity `adsb-icao` and DF/TC/altitude/CPR/velocity metadata. The rows attach to that aircraft's emitter, and the mapping's `"service": "adsb"` makes ADS-B its top explanation (ADR-0011 §2.2, `docs/api.md` "Messages outputs"). The field checks in the acceptance tests still read the recipe's `frames` inspector stream, the same way Tutorial 1 reads `groups`; the blind test then checks the stored rows and explanations.

## 3. Run it (API)

```sh
HK_TOKEN=... hk serve --device mock:<adsb fixture>.sigmf-meta
```

1. Wait for a handful of `hk-detect/burst@…` detections (there is no `GET /api/detections` route yet; a developer would use the same `Repository` query the test does), take their median centre.
2. `curl -X POST $API/api/pipelines -d '{"recipe_id":"adsb","target":{"band":{"f_lo":<median-0.4e6>,"f_hi":<median+0.4e6>}}}'` → `201`, `channel {bandwidth_hz: 1600000, sample_rate_hz: 2000000}`.
3. Read `inspector/<p>/frames` (WebSocket or TCP `inspector/<p>/frames?token=…`). One record per frame:
   ```json
   {"type":"frame","crc_status":"valid","decoder":"recipe:adsb@1",
    "content":{"hex":"8d3c6444...","layers":{"nodes":[
      {"path":"df","value":17},{"path":"icao","value":3949636,"text":"0x3C6444"},
      {"path":"me.tc","value":19},
      {"path":"me.velocity.vew","value":123},{"path":"me.velocity.dew","text":"east"},…]}}}
   ```

## 4. How it is tested

`tests/e2e/tests/acceptance/tutorial_adsb.rs`, part of `acceptance_m0`:

```sh
HK_E2E_REQUIRE_SYNTH=1 HK_REQUIRE_FIXTURES=1 cargo test -p hk-e2e --test acceptance_m0 tutorial_adsb -- --nocapture
```

- **`tutorial_adsb_recipe_decodes_blind_and_matches_truth`** (always runs). The 64-squitter synthetic scene (4 aircraft × 16 messages each, cycling identification/position-even/position-odd/velocity) replays through the mock SDR with its truth stripped; the band is found blind (§2). Two assertions:
  - **Correctness (never relaxed): 0 field mismatches, every run.** Whenever the recipe decodes a squitter CRC-valid, its ICAO, DF, type code and altitude/raw-CPR/velocity fields (whichever the message carries) match the hidden truth exactly.
  - **Coverage: every aircraft found, all 16 distinct truth squitters decoded.** All 4 ICAOs are found (by decoded identity — still blind), and every one of the 16 distinct (ICAO, type code, even/odd) truth squitters decodes CRC-valid at least once.
- **`tutorial_adsb_recipe_agrees_with_readsb`** additionally starts the built-in `adsb-readsb` plugin chain on the *same* live run (it attaches automatically — default chains — so readsb decodes independently of the recipe above) and compares ICAOs and altitude/velocity fields both sides decode (every one must agree), and checks the recipe decodes every (ICAO, type code) readsb decodes. Skips cleanly (as `signal_001_adsb_readsb_plugin_chain` does) when `readsb` isn't on `PATH` (e.g. `/opt/homebrew/bin`) or the `hk-plugin-readsb` wrapper isn't built next to the test binary (`cargo build -p hk-plugins --bins`; `just acceptance` does this). T-097's run skipped for the second reason, not because readsb was missing.
- **`hil_1090mhz_adsb_recipe_live_on_the_hackrf`** is `#[ignore]`d: deferred live HIL, blocked on a 1090 MHz antenna (T-025).

### The coverage gap: T-097's finding and T-110's root cause

T-097 decoded only 12 of 16 distinct squitters. The missing four (a0b1c2 and 4ca853, each position-even and velocity) never decoded, and every decoded combination succeeded on only 2–8 of its 8 tries. T-097 ruled out noise floor, per-aircraft power/CFO, message kind, and the CRC / `length_from` / field-map layout (bit-exact against `py/hkpy/synth/adsb.py`). Its bandwidth/rate sweep (including 2.4/2.0 MHz) was run against the old block, which is why it looked inconclusive.

**Root cause (T-110): `ppm_demod` sampled chips only at whole-sample positions.** It read chip `k` at `round(k·spc)` over `floor(spc)` samples, from a frame origin at the detecting integer sample (or the next one). The synthetic squitters start on integer 2.4 Msps samples, so after the channel resampler each one lands at its own sub-sample phase, and in a looping replay that phase is the same on every pass. At about one sample per chip, a burst half a sample off the grid puts every sample on a chip boundary, and the band-limited early/late comparison then depends on the neighbouring bits. A fixed (squitter, phase) pair either always survives or never does, which is what looked like a content-dependent bug. At 2.4 Msps (1.2 samples per chip) the rounding drifts across the frame, so the old block was even worse there.

Measured on the seed-5 fixture offline (numpy emulation of the channel, 64 squitter instances, exact-bit decodes):

| Channel | Whole-sample chips (old block) | Interpolated chip centres, phase by decision margin (new block) |
|---|---|---|
| 2.0 Msps / 1.9 MHz | 38 / 64 | 47 / 64 (best phase 47) |
| 2.4 Msps / 2.0 MHz | 0 / 64 | **64 / 64** |
| 4.0 Msps / 3.0 MHz | 50 / 64 | 62 / 64 (best phase 64) |

In the real pipeline, 2.0 Msps / 1.6 MHz still gives 12/16 with the new block (53 CRC ok), while 2.4 Msps / 2.0 MHz gives 16/16. Both changes are needed. The block's regression test, `band_limited_squitters_decode_at_every_sub_sample_phase`, renders the four never-decoded squitters band-limited to ±1 MHz at 2.4 Msps, at every tenth-sample phase, and requires all 40 bit-exact.

### Results of the last run (T-110)

| | Value |
|---|---|
| Band found blind | ≈1089.99 MHz (within tens of kHz of the recording's 1090.000 MHz), 2.4 Msps / 2 MHz channel |
| Distinct truth squitters decoded | **16 / 16**, each on 8 of 8 tries over two loop passes (all 128 truth instances CRC-valid) |
| CRC-valid rate | 127 / 656 = 0.194 of all emitted frames (`corrected_bits` 0, correction is off, §1). Every truth squitter is valid; the invalid frames are preamble false triggers on noise between squitters (random DFs), which the `crc` stage rejects. |
| Field mismatches | 0 |
| readsb agreement (same live run, three passes) | altitude/velocity fields 339 / 339 agree; readsb 451 rows over all 4 ICAOs; the recipe decodes every (ICAO, TC) readsb decodes (12 / 12, none readsb-only); recipe CRC 192 ok |
| T-097 (before) | 12 / 16 squitters, 54 / 226 CRC-valid, readsb comparison skipped |

The two tutorial tests plus `signal_001` take about 15 s together.
