# Tutorial 3: ACARS from blocks

M1, T-096; [docs/13](../13-m1-decoder-workbench.md), [ADR-0011](../adr/0011-decoder-workbench-contracts.md) §5.1. ACARS (ARINC 618 VHF downlink) decoded from built-in blocks by [`recipes/acars.recipe.json`](../../recipes/acars.recipe.json): AM carrier → MSK 2400 Bd on an 1800 Hz subcarrier → SYN/SOH..ETX framing → CRC-16 block check → fields (registration, label, block id, text).

**Status: the chain is verified correct offline; blind acceptance through the mock SDR is not yet green.** There is no real ACARS recording and no `acarsdec` oracle (T-098: not cheaply installable, and none was found in this environment either — `which acarsdec` fails). The only fixture is T-098's synthetic `acars_message` scene. This tutorial's blind-acceptance test currently fails, not on decoding, but earlier: the general blind detection/tracking pipeline never registers an inventory entry for the synthetic AM+MSK burst, so `POST /api/pipelines` never has a target to attach the recipe to. That's outside T-096's scope (recipe/block correctness) and is left for a follow-up task.

## 1. What was wrong with the recipe skeleton, and how it was found

The skeleton (`recipes/acars.recipe.json` before T-096) assumed real ACARS conventions that don't match T-098's synthetic generator (`py/hkpy/synth/acars.py`), which documents itself as "this project's own best-effort reading… not verified against a third-party decoder":

| Assumption in the skeleton | What the synthetic fixture actually sends | Fix |
|---|---|---|
| Differential decoding (`diff_decode`, MSK "bits are differential") | Mark/space tone maps straight to bit 1/0 — no differential coding. Verified bit-exact by the existing T-086 unit test `acars_path_am_subcarrier_msk_recovers_hidden_bits`, which recovers the hidden bits with **no** `diff_decode` step. | Dropped the node. |
| Sync word `'+' '*' SYN SYN SOH` = `0xD554686880` (40 bits), `bit_order: lsb` | No `+*` preamble; framing is 32 alternating clock-sync bits, then `SYN SYN SOH` = `0x16 0x16 0x01` raw, each character (including the parity-bearing ones after SOH) sent **parity bit first, then the 7 data bits MSB→LSB** — not LSB-first. | Sync word `0x160116` over 24 bits, no `bit_order`. |
| CRC-16/KERMIT (`refin`/`refout` true) | `py/hkpy/synth/acars.py`'s `crc16_acars` is CRC-16/**XMODEM** (poly `0x1021`, init 0, **not** reflected) — confirmed against `hk-estimate`'s CRC catalogue. | `refin`/`refout` false. |
| — (unnoticed) | The block check is computed over the **pre-parity** 7-bit values, each padded back to 8 bits with the parity position forced to **0** — not over the as-sent bytes (their real parity bit) and not over a parity-stripped, narrower, byte-misaligned repacking. Checked numerically against the generator (`py3 -c 'from hkpy.synth import acars…'`): only the zero-padded convention reproduces the transmitted CRC trailer. | New `parity` block mode, `zero: true` (below). |

Empirical check (not guesswork): computing the CRC three ways over one built frame — (a) python's own zero-padded body, (b) the as-transmitted bytes with real parity, (c) parity-stripped and repacked with no padding — only (a) reproduced the transmitted trailer (`0x9435` for the worked example); (b) gave `0x63ab`, (c) gave `0x784f`.

## 2. A new `parity` mode: `zero`

`fields` needs each character's **real** parity bit (it does its own per-character odd-parity check, `crates/hk-recipe/src/fields/eval.rs`); the block check needs it **replaced with a constant 0**, width unchanged, so the two can't share one transform. `parity` (`crates/hk-blocks/src/blocks/fec/parity.rs`) already had `strip` (remove the parity bit, narrowing the frame); T-096 added `zero` (replace it in place, same width), exclusive with `strip`. Unit test: `crates/hk-blocks/src/blocks/fec/tests.rs::parity_zero_then_crc_matches_pre_parity_check_t096` — parity-zeroes a real captured body-with-parity vector, then CRC-16/XMODEM-checks it and asserts `Valid` and byte-for-byte agreement with the pre-parity body.

## 3. The chain, stage by stage

```text
input(iq, 24 kS/s) → am → tone → msk → clock → slice → sync ─┬→ unwrap → msg
                     iq   real   iq    real   soft  bits     └→ pz → crc
```

`sync` (bits → frames) feeds two branches:
- `unwrap` (`crc`, real parity bits kept) → `msg` (`fields`): the block-check width is stripped either way, so `msg` sees the correct body regardless of whether the check validates.
- `pz` (`parity`, `zero: true`) → `crc`: the block-check status the recipe's `refine` objective (`crc.error_rate`, minimise over `center_hz`) and this tutorial's CRC-valid rate read from.

| Node | Block | What it does |
|---|---|---|
| `am` | `am_demod` | Envelope detector: IQ → AM audio. |
| `tone` | `subcarrier` (1800 Hz, 2.4 kHz wide, 12 kS/s out) | Mixes the 1800 Hz tone band down to baseband. |
| `msk` | `msk_demod` (2400 Bd) | Non-coherent discriminator, deviation = symbol_rate/4 = 600 Hz: mark (2400 Hz) → +1, space (1200 Hz) → −1. |
| `clock` | `clock_recovery` (2400 Bd, `nrz`, `gardner`) | Symbol timing. |
| `slice` | `slicer` (threshold 0) | Soft symbols → hard bits, straight (no inversion, no differential decode). |
| `sync` | `sync_search` (`0x160116`, 24 bits, terminator ETX `0x83`/ETB `0x97` + 16 trailer bits) | Finds `SYN SYN SOH`, cuts the frame at the first parity-coded ETX/ETB plus the 16-bit check. |
| `unwrap` | `crc` (CRC-16/XMODEM, `strip`) | Strips the trailer unconditionally (its own `ok` flag is not meaningful here — the *real* parity bits don't satisfy the zero-padded convention). |
| `msg` | `fields` (`acars_block` map) | mode, registration, ack, label, block id, stx/etx, text — each field its own 7-bit-+-odd-parity ASCII characters. |
| `pz` | `parity` (`zero: true`, `position: first`, trim 16 trailer bits) | Replaces each character's parity bit with 0 in place. |
| `crc` | `crc` (CRC-16/XMODEM, `strip`) | The real block-check status. |

## 4. What is verified, and how

- **Structural**: `crates/hk-blocks/tests/acars_recipe.rs` validates the recipe against the pinned M1 block catalogue (every block, parameter, port type) and checks the sync word, CRC parameters and `parity`/`zero` wiring match the synthetic generator's constants — both pass.
- **Bit-level**: `crates/hk-blocks/src/blocks/fec/tests.rs::parity_zero_then_crc_matches_pre_parity_check_t096` checks the `parity`(`zero`) → `crc` combination against a hand-computed vector from the actual generator (§1); `crates/hk-blocks/src/blocks/iq/tests.rs::acars_path_am_subcarrier_msk_recovers_hidden_bits` (T-086, unchanged) already covers `am_demod → subcarrier → msk_demod → clock_recovery → slicer` end to end with hidden random bits.
- **Not yet verified**: the full recipe through the mock SDR. `tests/e2e/tests/acceptance/tutorial_acars.rs` (`#[ignore]`, reason logged in the test) drives the T-098 `acars_message` synthetic scene through `hk serve`-style API wiring exactly as [Tutorial 1](01-rds.md) does, but `found_blind` — matching `/api/inventory` against the fixture's hidden truth — times out with an **empty** inventory after looping the (short, bursty) recording for 240s. This is upstream of the recipe: detection/tracking never creates an emitter for this burst at all, so there is nothing to `POST /api/pipelines` against. Candidates for the follow-up: the burst may be too short relative to the tracker's dwell/confidence window, or 131.55 MHz / the `am` family may not be covered by whatever chain set the mock's default `ScanPlan` instantiates. Worth checking against the ADS-B tutorial's (T-097) detection path, which also has a bursty, non-continuous signal.

```sh
# Passing today:
cargo nextest run -p hk-blocks --test acars_recipe
cargo nextest run -p hk-blocks -E 'test(parity_zero_then_crc_matches_pre_parity_check_t096)'

# Currently #[ignore]d (blind detection doesn't find the emitter; see §4):
HK_E2E_REQUIRE_SYNTH=1 HK_REQUIRE_FIXTURES=1 cargo test -p hk-e2e --test acceptance_m0 tutorial_acars -- --ignored --nocapture
```
