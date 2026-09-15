# Tutorial 4: ADS-B from blocks

M1, T-097; [docs/13](../13-m1-decoder-workbench.md), [ADR-0011](../adr/0011-decoder-workbench-contracts.md). Decodes 1090 MHz Mode S extended squitters (ADS-B DF17) using built-in blocks wired by a recipe: [`recipes/adsb.recipe.json`](../../recipes/adsb.recipe.json).

Unlike [Tutorial 1 (RDS)](01-rds.md), the target here is not found through `/api/inventory`: ADS-B squitters are ~120 µs bursts, and burst detections never cluster into a stable inventory emitter on their own (see §2). So this tutorial finds the band from the burst detector's own blind measurements directly, then attaches the recipe to that band. Everything below replays the synthetic `adsb_squitter` scenario (4 aircraft × squitters, 2.4 Msps at 1090 MHz); a real 1090 MHz capture is blocked on the antenna (T-025), same as `SIGNAL-001`.

## 1. The chain, stage by stage

```text
input(iq, 2 MS/s, 1.6 MHz) → ppm → crc → msg(fields) → frames (inspector)
```

| Node | Block | What it does |
|---|---|---|
| `ppm` | `ppm_demod` (`bit_rate_bd` 1 000 000, `chips_per_bit` 2, preamble `0xA140` over 16 chips, `min_snr_db` 9, `length_from`: DF (bits 0–4) 16–31 → 112 bits, else 56) | Scans for the 8 µs preamble (pulses at chips 0, 2, 7, 9), then decodes each data bit as early-chip-vs-late-chip energy (PPM: bit 1 = pulse-then-quiet, bit 0 = quiet-then-pulse). The frame ends at 56 or 112 bits depending on the downlink format it reads as soon as those bits arrive. |
| `crc` | `crc` (width 24, poly `0xFFF409`, `span: {start_bit: 0, end_trim_bits: 0}`, `strip: false`) | Checks the whole frame's CRC-24 (Mode S parity, MSB-first, no reflection). No burst correction (`correct_burst_bits` left unset): matches readsb's own `--no-fix` choice — see §4. DF 0/4/5/20/21 overlay the ICAO address on the parity field, so those frames' `crc_status` fails by design; this fixture and recipe only exercise DF17. |
| `msg` | `fields` (`map: adsb_frame`) | DF, `ca`; ICAO (DF 11/17/18); for DF17/18, the ME field: `tc` (type code) then, conditioned on `tc`, identification (category + raw 48-bit callsign bytes), airborne position (surveillance status, altitude — AC12 with its Q-bit `skip_bits` out, `scale: 25, add: -1000` → ft — and the two raw 17-bit CPR ints), or airborne velocity (`vew`/`vns` magnitudes plus `dew`/`dns`/`svr` direction enums, and vertical rate `scale: 64, add: -64`). |

The `adsb_frame` field map's bit layout was checked bit-for-bit against `py/hkpy/synth/adsb.py`'s encoder (`me_identification`, `encode_altitude`, `me_airborne_position`, `me_velocity`) while tuning this recipe — every offset, `skip_bits` position and `scale`/`add` lines up exactly, which is why every CRC-valid frame's fields matched the hidden truth exactly in every run below (0 mismatches, always).

**Recipe parameters tuned in T-097** (the skeleton's placeholders didn't decode at all):
- `input.bandwidth_hz` was 2 000 000, equal to `input.sample_rate_hz` — the DDC's own planner rejects a channel with **zero** transition-band headroom (`bandwidth/2 < stopband ≤ rate − bandwidth/2` fails at equality), so every attempt to start the recipe returned `422 unrealisable`. Lowered to 1 600 000 Hz.
- `ppm.min_snr_db` was left at the block's default (6 dB); raised to 9 dB, which measurably cut false preamble triggers on noise without reducing genuine recall (identical real-squitter decode counts at 6 and 9 dB across repeated runs).
- `ppm.correct_burst_bits` is deliberately **not** set. CRC-24 over a 112-bit frame allows 1-bit correction within the false-correction bound (ADR-0011 §5.1, `crates/hk-blocks/src/blocks/fec/crc.rs`), but `hk-plugin-readsb`'s own module doc records that readsb's `--fix` (its default) prints repaired-but-wrong frames whose recomputed CRC then checks out — measured there as 20/20 lines with `--fix` vs 10/10 genuinely good with `--no-fix` on the same input, which is why the wrapper always passes `--no-fix`. Recipe correction is left off for the same reason: a valid-looking wrong ICAO/position is worse than a missed message.

## 2. Finding the target blind

RDS's continuous FM carrier turns into a stable `/api/inventory` emitter from spectral peak tracking alone. ADS-B squitters don't: `hk-pipeline/src/detect.rs`'s burst path (`hk-detect/burst@…`) stores its detections but never hands them to the tracker ("untracked"), so no `Sighting`/`Emitter` is ever created from bursts alone — confirmed by `SIGNAL-001`'s burst-path test, which finds every squitter blind via raw `Detection` rows and notes "untracked rows (no emitter)". An emitter only appears once a decoder (readsb) resolves an ICAO identity.

So the tutorial (and both acceptance tests) instead:
1. Start the live run through the mock SDR as usual (`hk serve`-shaped: the composed pipeline's burst-detection reader runs regardless of any recipe).
2. Poll `Repository::detections_in_region` for `hk-detect/burst@…` rows (this is the system's own blind RF measurement — no database, no truth, no band-plan lookup) until there are enough, then take their **median** centre frequency.
3. `POST /api/pipelines {"recipe_id": "adsb", "target": {"band": {"f_lo": median-0.4MHz, "f_hi": median+0.4MHz}}}`. Only the band's centre matters: `channel_plan` always uses the recipe's own fixed `input.bandwidth_hz` for the actual channel.

On the synthetic fixture this reliably lands within tens of kHz of 1090.000 MHz, comfortably inside the FM/ADS-B band-plan prior's `unrestricted` window and the mock's tuned Nyquist.

**Known gap (found by T-097, not fixed here — a runtime feature, not a block bug).** The recipe's `aircraft` output (`kind: "messages"`) is meant to ingest decodes into the Repository the way readsb's own decodes do, giving the recipe its own `/api/inventory` emitters once it starts decoding ICAOs. At runtime this is still a no-op: `hk-pipeline/src/recipes/runtime.rs`'s `build_sink` answers `OutputKind::Messages => Ok((OutputSink::Idle, None))`, and `recipes/graph.rs` still emits the warning "messages outputs are served once field-map evaluation lands (T-089)" even though T-089 (the field-map evaluator) has landed. So both acceptance tests below read the recipe's `frames` inspector stream directly, the same way Tutorial 1 reads `groups`.

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
  - **Coverage: every aircraft found, ≥ 12 of 16 distinct truth squitters decoded.** All 4 ICAOs are found (by decoded identity — still blind) every run. Of the 16 distinct (ICAO, type code, even/odd) truth squitters, at least 12 decode CRC-valid at least once over the run's independent tries.
- **`tutorial_adsb_recipe_agrees_with_readsb`** additionally starts the built-in `adsb-readsb` plugin chain on the *same* live run (it attaches automatically — default chains — so readsb decodes independently of the recipe above) and compares ICAOs and altitude/velocity fields both sides decode. Skips cleanly (as `signal_001_adsb_readsb_plugin_chain` does) when `readsb` or the `hk-plugin-readsb` wrapper binary isn't available (both absent in this environment; only structurally verified here).
- **`hil_1090mhz_adsb_recipe_live_on_the_hackrf`** is `#[ignore]`d: deferred live HIL, blocked on a 1090 MHz antenna (T-025).

### The coverage gap: what T-097 found

The remaining 4 of 16 (a specific pair of aircraft × {position-even, velocity}) stay at 0 CRC-valid decodes even over 80 independent tries (`messages_per_aircraft`), while every *other* combination for those same two aircraft, and every combination for the other two aircraft, decodes reliably (tens of hits). The investigation ruled out, in order:

| Hypothesis | Test | Result |
|---|---|---|
| Wrong DDC bandwidth/rate pairing | Swept `sample_rate_hz`/`bandwidth_hz` (2.0/1.6, 2.4/2.0, 2.0/1.2, 2.0/1.8 MHz) | 1.6 MHz was the best of these; wider *and* narrower were both worse — ruled out as the dominant factor |
| Noise-floor-limited | `noise_dbfs` swept −35 → −70 dBFS | No change to the missing set — ruled out |
| Aircraft power/CFO alone | Forced equal power (−6 dBFS) and zero CFO for all 4 aircraft | Made overall recall *worse*, not better — ruled out |
| Message-kind-specific (e.g. "identification always fails") | Raised `messages_per_aircraft` 4→8→16→40→80 | At low counts identification looked cursed (0/4 every time); at higher counts it decoded fine — a sampling artefact, not a real kind dependency |
| CRC / `length_from` / field-map bug | Re-derived the `adsb_frame` map's bit layout against `py/hkpy/synth/adsb.py` by hand; DF is constant (17) so `length_from` can't vary by content | Bit-exact match; 0 field mismatches in every configuration tried |
| Wrong content class / band-plan classification | Investigated separately (see below) | Real, but a different issue (fixed by keeping the *target*'s own span narrow — see §2's `±0.4 MHz`, not the channel) |

What's left is a genuine, content-dependent (specific bit pattern) decode-margin limitation of `ppm_demod`'s early/late chip-energy comparator, most plausibly inter-symbol interference on long runs of one chip polarity, interacting with the *band-limited* (not ideal-rectangular) synthetic pulse shape. Confirming that precisely, and fixing it — likely matched filtering or multi-sample-per-chip integration in `ppm_demod` — is real DSP work, not a minimal, isolated block change, so it's filed as a follow-up rather than attempted here.

### Results of the last run

| | Value |
|---|---|
| Band found blind | ≈1089.99 MHz (within tens of kHz of the recording's 1090.000 MHz), 1.6 MHz channel |
| CRC-valid rate | 54 / 226 = 0.239 (`corrected_bits` 0 — correction is off, §1) |
| Distinct truth squitters decoded | 12 / 16 (all 4 aircraft; the 4 gaps are documented above) |
| Field mismatches | 0 |
| readsb agreement | not run in this environment (readsb absent); structurally verified (clean skip) |

Both tests together take about 25 s.
