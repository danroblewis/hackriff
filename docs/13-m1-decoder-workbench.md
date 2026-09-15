# M1 — Decoder workbench: build decoders inside hackriff

Status: **brief for the coordinator** (2026-09-15, from the user via the supervisor). This supersedes the old M1 row in `docs/11-roadmap.md` ("wrap existing decoders as plugins"). Read this, turn it into tasks in `docs/tasks.yaml`, update the M1 row in `docs/11`, and fan out. Commit per task.

## The re-scope

The user does **not** want a Rust module written per protocol. Decoders are **built inside the exploratory system** from reusable blocks and **declarative recipes**, discovered from the signal, not coded from a spec. External decoders (rtl_433, readsb) stay only as a long-tail escape hatch and as **test oracles**, never the headline path.

This is the M1 the mockups (`scratchpad/hackriff-explorer.html`, v3) illustrate: parallel decode **pipelines**, a **Wireshark-style packet inspector** over a byte stream, hop/multi-channel following, and always-on capture.

Core principles (unchanged): exploration-first, blind detection before any database, tune from the processed output, outputs feed other programs, e2e through the mock SDR device interface, UI stays a thin client over the documented API (`docs/api.md`).

## The model, in four layers

1. **Blocks** — a library of generic, reusable DSP/framing/FEC/parse primitives, written once in Rust, on the real-time path.
   - IQ/demod: `mix`, `lowpass`, `resample`, `fm_demod`, `am_demod`, `fsk_demod`, `msk_demod`, `ppm_demod`, subcarrier extraction.
   - Symbol: `clock_recovery`, `slicer`, `diff_decode`, `nrzi`, `manchester`.
   - Framing: `sync_search` (correlate a sync word/offset words), `deframe` (fixed/variable frames), `interleave`/`deinterleave`.
   - Error control: `crc` (configurable polynomial/width/init/reflect), `bch`, `parity`, `checksum`.
   - Parse: `fields` (see the declarative parser below), `text` (character codings).
   - Multi-channel: `follow_hops` (one pipeline spanning several channels; carries source channel into each frame).
   Each block: typed inputs/outputs, parameters, a status/quality readout, and a stage-output stream the UI can render. **This block contract is a core interface** — Opus, reviewed before merge.

2. **Recipes** — a decoder is **data, not code**: a named recipe chains blocks with parameters and a field layout. Runs live in the backend; edited without rebuilding capture (the ADR-0001 "reconfigure without stopping capture" requirement). Recipes are saved, listed, and re-run on any matching signal. **Recipe schema is a core interface** — Opus, reviewed.

3. **Declarative parser + packet inspector** — the `fields` block interprets a byte record against a **field map**: `{name, offset, length (bytes or bits), type (uint/int/enum/ascii/bitfield), endianness, condition}`, nestable into layers. The **inspector** (the mockup's bottom pane) renders the byte stream as: a **frame list**, **hex + ASCII**, and a **layer tree**, with **bidirectional linked selection** (click a field → highlight its bytes; click a byte → select its field). The output stream is a **sequence of byte records** served over the existing stream contract (`tcp :8788`, `/ws`), one record per frame. **Parser schema + inspector stream framing are core interfaces** — Opus, reviewed.

4. **Parser authoring workflow** (the research payoff) — build a parser over a recording:
   - Point a demod pipeline at an unknown signal; it recovers a bitstream/byte stream before the protocol is known.
   - That stream is **always recorded** (always-on capture, extended to decoded streams).
   - In the inspector, mark up fields on the recording; the parser re-runs against the whole recording instantly, showing whether the guess holds across many frames.
   - **Assist tooling** (classical, not ML): sync-word / period hunting, entropy-based field-boundary guessing, and CRC/BCH brute-forcing against the recorded frames. These are suggestions the user accepts/edits — never a source of truth.
   - Save when it holds; it then runs live.

## Scope in / out for M1

- **In:** the block library above; the recipe runtime; the declarative parser; the packet inspector; parser-authoring assist (sync/period/entropy/CRC-search); always-on decoded-stream capture with scrubbable review; parallel pipelines; `follow_hops` (one pipeline, several channels); the four tutorials below.
- **Out (later milestones):** cross-channel **fusion** where decoding one stream steers another (trunking control→voice) → **M4**. ML classification → M3. New RF accessories → M5.

## Tutorials (acceptance-driving, in order)

Each is a saved recipe plus a blind acceptance test through the mock SDR (hidden ground-truth), and where possible checked against an existing oracle.

1. **RDS — data on FM** (native, on air now at 101.3 MHz). fm_demod → 57 kHz subcarrier → BPSK 1187.5 Bd → diff_decode → block sync (offset words A–D, CRC 0x5B9) → fields (PS, RadioText, PI, PTY). Oracle: the existing Rust RDS decoder. This is the first tutorial and the reference for the whole workbench.
2. **POCSAG — pager messages.** 2-FSK 512/1200/2400 → sync 0x7CD215D8 → BCH(31,21)+parity → fields (RIC address, function, numeric/alphanumeric text). Demonstrates `follow_hops` across a multi-channel pager net.
3. **ACARS — aircraft messages.** AM → MSK 2400 Bd → SYN/STX framing → CRC-16 → fields (registration, label, block id, text).
4. **ADS-B — aircraft positions.** PPM 1 Mbit/s on 1090 MHz → preamble → 56/112-bit frames → CRC-24 → fields (ICAO, position, velocity). Oracle: readsb. **Blocked on a 1090 MHz antenna** (user); build and test through the mock device from a recording, mark the live HIL part deferred.

## Test strategy

- Blind ground-truth through the **mock SDR device** (per CLAUDE.md); never look a frequency up and tune there.
- The declarative parser must run against **recorded** streams so a recipe is provable in CI without hardware.
- Existing decoders (RDS, readsb) are oracles: assert the recipe's fields match the oracle on the same fixture.
- Fixtures: RDS from the live capture; POCSAG/ACARS/ADS-B from public SigMF samples or user captures (licences not a gate).

## Suggested task fan-out (coordinator finalises IDs/deps in tasks.yaml)

Do the **contract design first**, then parallelise. Mark `parallel_group`s so worktrees don't collide.

- **M1-DESIGN (Opus high, core_interface, reviewed):** one doc/ADR fixing the block contract, recipe schema, parser field-map schema, and inspector stream framing. Blocks the rest. Small worked example: the RDS recipe expressed in the schema.
- After it lands, parallel groups:
  - **Blocks A** (demod/symbol): mix/lowpass/resample already exist in hk-dsp — wrap to the block contract; add slicer/diff_decode/nrzi/manchester. (Opus/Sonnet, real-time path → reviewed.)
  - **Blocks B** (framing/FEC): sync_search, deframe, crc/bch/parity. (Opus, reviewed.)
  - **Recipe runtime** (hk-pipeline): load/run/hot-edit a recipe as a chain; save/list; serve records over the stream contract. (Opus high, core.)
  - **Parser + inspector API** (hk-api + docs/api.md): field-map evaluation, frame records, contract tests. (Opus/Sonnet.)
  - **Inspector UI** (ui/, thin client): frame list, hex+ASCII, layer tree, linked selection. (Sonnet, over docs/api.md.)
  - **Authoring assist** (hk-estimate/hk-detect): sync/period hunt, entropy boundaries, CRC/BCH search over recorded frames. (Opus high, novel DSP.)
  - **Always-on decoded capture + scrub** (hk-store + api + ui): record decoded streams; scrub/review. (Opus/Sonnet.)
  - **follow_hops** (hk-core/hk-pipeline): one pipeline over several channels, channel tag in each frame. (Opus, core.)
  - **Tutorials 1–4**: each a recipe + blind acceptance test; RDS first as the reference; ADS-B live part deferred on the antenna.

Keep 4–6 agents busy once M1-DESIGN merges. Log blockers (1090 MHz antenna) in `docs/planning-log.md` and keep unblocked work moving.
