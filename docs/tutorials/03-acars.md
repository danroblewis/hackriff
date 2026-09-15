# Tutorial 3: ACARS from blocks

M1, T-096/T-108; [docs/13](../13-m1-decoder-workbench.md), [ADR-0011](../adr/0011-decoder-workbench-contracts.md) §5.1. ACARS (ARINC 618 VHF) decoded from built-in blocks by [`recipes/acars.recipe.json`](../../recipes/acars.recipe.json): AM carrier → MSK 2400 Bd on an 1800 Hz subcarrier → MSK chips → `+* SYN SYN SOH`..ETX framing → CRC-16/KERMIT → fields (registration, label, block id, text).

**Status (T-108):** the recipe, the synthetic fixture and its reference decoder follow the conventions read from acarsdec's receiver source (§1). The blind acceptance test `tests/e2e/tests/acceptance/tutorial_acars.rs` runs and passes (§4). Unverified: no real ACARS capture yet, and `acarsdec` isn't installed here, so the oracle branch is still skipped.

## 1. The real convention, from acarsdec's source

T-096 had bent the recipe to match T-098's synthetic generator: MSB-first characters, direct mark/space bits, CRC-16/XMODEM over parity-zeroed characters, plus a new `parity` `zero` mode. The generator itself said it was "not verified against a third-party decoder", and it was wrong. T-108 took the conventions from **acarsdec** ([github.com/TLeconte/acarsdec](https://github.com/TLeconte/acarsdec), commit `339f63e`), the reference open-source receiver:

| Question | acarsdec source | Convention |
|---|---|---|
| Bit order in a character | [`msk.c` L53–63](https://github.com/TLeconte/acarsdec/blob/339f63eb91a890cfe5b199ad70814cfe86702d1e/msk.c#L53-L63) `putbit`: `outbits >>= 1; if (v > 0) outbits \|= 0x80;` | Each new bit enters at the top, so the first bit on air ends as the LSB. **Characters are sent LSB first**, with the parity bit (bit 7) last. |
| MSK bits | [`msk.c` L74–131](https://github.com/TLeconte/acarsdec/blob/339f63eb91a890cfe5b199ad70814cfe86702d1e/msk.c#L74-L131) `demodMSK`: VCO at 1800 Hz advancing 3π/2 per bit, half-sine matched filter, decisions alternating `Re`, `Im`, `−Re`, `−Im` (`MskS`) | Coherent (offset-QPSK) MSK: **the decisions are the chips, and the chips are the data**. Relative to the VCO the phase moves +π/2 per bit (2400 Hz) when a chip equals the previous one and −π/2 (1200 Hz) when it changes. The tone therefore marks chip *transitions*; the [WAVECOM summary](https://cartoonman.github.io/WAVECOM/wavecomhtm/acars.htm) calls this "NRZI coded coherent … MSK". |
| Polarity | [`acars.c` L252–277](https://github.com/TLeconte/acarsdec/blob/339f63eb91a890cfe5b199ad70814cfe86702d1e/acars.c#L252-L277): SYN or `~SYN` (then `MskS ^= 2`) | Chip polarity is ambiguous, so the receiver accepts either. |
| Sync / preamble | [`acars.c` L22–27, L252–300](https://github.com/TLeconte/acarsdec/blob/339f63eb91a890cfe5b199ad70814cfe86702d1e/acars.c#L22-L27): sliding bit-by-bit search for SYN (0x16), SYN, then SOH (0x01) | `SYN SYN SOH`. ARINC 618 (WAVECOM summary) puts a pre-key (16 characters of ones) and bit sync `+ *` before it, and a DEL after the BCS. |
| Frame end | [`acars.c` L303–349](https://github.com/TLeconte/acarsdec/blob/339f63eb91a890cfe5b199ad70814cfe86702d1e/acars.c#L303-L349): characters after SOH are stored *as received* until `ETX 0x83` / `ETB 0x97`, then two BCS bytes | The terminators are the parity-bearing values (0x03 and 0x17 with odd parity). |
| CRC variant | [`syndrom.h` L15–49](https://github.com/TLeconte/acarsdec/blob/339f63eb91a890cfe5b199ad70814cfe86702d1e/syndrom.h#L15-L49): `crc_ccitt_table[1] = 0x1189`; `update_crc(crc,c) crc = (crc>>8) ^ table[(crc^c)&0xff]` | Reflected table for poly 0x1021 (0x8408), init 0, xorout 0: **CRC-16/KERMIT**. |
| CRC span and parity | [`acars.c` L158–165](https://github.com/TLeconte/acarsdec/blob/339f63eb91a890cfe5b199ad70814cfe86702d1e/acars.c#L158-L165): `crc = 0`, then `update_crc` over every `txt[i]`, then `crc[0]`, `crc[1]`; zero = valid | The span is the characters after SOH through ETX/ETB, **parity bits included** (parity is only stripped afterwards, L194–200). The zero residue means the **BCS is sent low byte first**. |

## 2. What changed

- **Synth** (`py/hkpy/synth/acars.py`):
  - LSB-first characters.
  - Pre-key of 128 one-chips, then `+ * SYN SYN SOH`.
  - CRC-16/KERMIT over the parity-bearing characters, BCS low byte first, then DEL.
  - MSK tones from chip transitions (`msk_tones`).
- **Scene** (`acars_message` in `scenarios.py`): the emission moved off the tuned centre (`channel_offset_hz` 50 kHz, 192 kS/s) and repeats (`n_bursts`, `period_s`); see §3.
- **Reference decoder** (`py/tests/test_synth.py`): a port of acarsdec's chip decisions (1800 Hz mix, half-sine matched filter, `Re/Im/−Re/−Im`, either polarity) and its table-driven CRC residue check, independent of the generator's tone mapping. A generator that got the tone mapping, bit order or CRC wrong would fail it.
- **Blocks**, both needed for real ACARS, not for a synth quirk:
  - `nrzi` gains `direction: encode`: a running level, so a non-coherent discriminator's tone bits integrate back to chips.
  - `sync_search` gains `polarity: either`: the complemented word also syncs and complements its frame, like acarsdec's `~SYN`.
  - T-096's `parity` `zero` mode and its test are removed.
- **Recipe**: `… slicer → chips (nrzi encode, transition-is-0) → sync (0xD554686880, 40 bits, lsb, either) → crc (CRC-16/KERMIT) → msg (fields)`.

## 3. Why blind detection never found the burst

The T-098 scene put the ACARS emission at **channel offset 0**, exactly on the recording's tuned centre. A ~5 kHz emission there fits the detector's DC rule: rule 2 `dc_hit` in `crates/hk-detect/src/rules.rs`, which flags anything ≤ 40 kHz wide within 15 kHz of the centre. On a HackRF that spot is LO leakage, so every detection was rejected as a DC spur and `/api/inventory` stayed empty. The rule is right and the scene was wrong: nobody tunes an SDR's centre onto the channel they want. Burst length, the short-burst profile and SNR were not the problem.

The scene now:
- sits 50 kHz off centre at 192 kS/s, like `fsk_burst_train`'s 50 kHz offset;
- sends the block three times, 0.6 s apart, with the carrier off between, as a station repeating a block would.

No detection threshold changed.

## 4. The chain, stage by stage

```text
input(iq, 24 kS/s) → am → tone → msk → clock → slice → chips → sync → crc → msg
                     iq   real   iq    real   soft   bits   bits   frames frames
```

| Node | Block | What it does |
|---|---|---|
| `am` | `am_demod` | Envelope detector: IQ → AM audio. |
| `tone` | `subcarrier` (1800 Hz, 2.4 kHz wide, 12 kS/s out) | Mixes the MSK tone band down to baseband. |
| `msk` | `msk_demod` (2400 Bd) | Non-coherent discriminator, deviation 600 Hz: 2400 Hz → +1, 1200 Hz → −1. |
| `clock` | `clock_recovery` (2400 Bd, `nrz`, `gardner`) | Symbol timing. |
| `slice` | `slicer` | Soft → hard: 1 = mark. |
| `chips` | `nrzi` (`direction: encode`, `transition-is-0`) | Mark = unchanged chip, so `level ⊕= NOT tone`: the coherent chips, up to polarity. A tone error flips the rest of that frame, the price of a non-coherent receiver. |
| `sync` | `sync_search` (`+* SYN SYN SOH` = `0xD554686880`, 40 bits, ≤ 2 errors, `polarity: either`, `bit_order: lsb`, terminator ETX `0x83` / ETB `0x97` + 16 trailer bits) | Frames from the mode character through the BCS, characters packed with parity in the top bit. |
| `crc` | `crc` (CRC-16/KERMIT, `strip`) | Checks the BCS over the parity-bearing characters, reading it little-endian; sets the frame's check status (the `refine` objective). |
| `msg` | `fields` (`acars_block`) | mode, registration, ack, label, block id, STX, text: each a 7-bit + odd-parity ASCII character. |

## 5. What is verified, and how

- **Convention**: `py/tests/test_synth.py::test_acars_message_demodulates_with_valid_crc` decodes every synthetic burst with the acarsdec-style chip decoder and a KERMIT residue check. `test_acars_crc_is_kermit_over_parity_bearing_chars` checks the CRC catalogue value (`"123456789"` → 0x2189) and the residue.
- **Blocks**:
  - `crates/hk-blocks/src/blocks/framing/tests.rs::acars_terminator_lsb_characters_and_crc16_kermit` feeds the recipe's `sync` and `crc` nodes a hand-built frame, in both polarities after tone integration.
  - `symbol/line.rs::diff_decode_and_nrzi_recover_encoded_bits` shows `nrzi` encode inverts decode.
  - `crates/hk-blocks/tests/acars_recipe.rs` validates the recipe against the pinned catalogue and asserts the conventions above.
- **Blind acceptance**: `tests/e2e/tests/acceptance/tutorial_acars.rs` replays the scene through the mock SDR with truth hidden, finds the emitter from `/api/inventory`, attaches the recipe by emitter id, and compares the decoded fields with the hidden truth. Results in §6.

```sh
cargo nextest run -p hk-blocks
(cd py && uv run pytest tests/test_synth.py -k acars)
HK_E2E_REQUIRE_SYNTH=1 HK_REQUIRE_FIXTURES=1 cargo test -p hk-e2e --test acceptance_m0 tutorial_acars -- --nocapture
```

## 6. Results

Blind run through the mock SDR (`tutorial_acars`, seed 96, 3 bursts per loop, 25 dB SNR), 2026-09-15:

| | Decoded (majority, share) | Hidden truth |
|---|---|---|
| Inventory | 1 of 1 rows match; emitter at 131.5493 MHz, 5 kHz | 131.550 MHz |
| mode | `2` (1.00) | `2` |
| registration | `.HKRF01` (1.00) | `.HKRF01` |
| label | `H1` (1.00) | `H1` |
| block id | `1` (1.00) | `1` |
| text | `HACKRIFF T096 TUTORIAL 3` (1.00, closing ETX stripped) | same |

- 25 block frames were streamed.
- CRC-valid rate (CRC-16/KERMIT, pipeline status): **19/19 = 1.000**.

**Content class.** 118–137 MHz has no content rule, so a frequency-derived class of `metadata-only` would gate the fields. The test vouches the recording `unrestricted` (`BlindSource.vouched_class`, user configuration, as in the listen/AWARE-053 tests). An aeronautical-VHF band prior is a possible follow-up.

## 7. Still open

- **Real signal.** No real ACARS capture has been decoded. A 131.550 MHz capture (antenna permitting) is the next check.
- **Oracle.** acarsdec isn't installed; the py decoder ports only its chip decisions and CRC.
- **Non-coherent vs coherent.** A tone error flips the rest of a frame after integration. A coherent MSK/OQPSK demodulator block would behave like acarsdec (one chip error, one bit error) if real captures need it.
