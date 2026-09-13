# C21 · bit-framing-inference
> Layer D — Demodulate & decode · Status: draft (taxonomy draft 2026-09-13) · Depends on: C20, C10, C18 · Used by: C18, C22, C24, C28, C39

## Purpose
Takes many bitstreams from one emitter cluster and infers the frame structure. It finds the line code, preamble, sync word, whitening, length and framing, CRC/checksum parameters, FEC family and field map, then emits a draft decoder spec that can be verified on held-out bursts. This is what makes "decode unknown signals" real rather than a decoder catalogue. Serves workflow step 6 (bitstreams from unknown signals) and hands decoders to step 7.

## Interface
- **In:** a `BitCorpus` (provisional): N `BitStream`s or `PulseList`s from C20 sharing a C10/C18 cluster id, with per-burst timestamps, RSSI, SNR, soft-bit confidence and polarity/rotation flags. Optional side information: time, user-entered sensor readings, printed device IDs (docs/04 §7.7 step 4).
- **Out:**
  - `FrameModel` (provisional): line code; preamble pattern and length; sync word with polarity; whitening LFSR (polynomial, seed); framing rule (fixed length, length field, terminator); FEC family and parameters.
  - `CrcModel`: width, polynomial, init, reflect in/out, xorout, covered byte range; or a sum/XOR checksum.
  - `FieldMap`: bit ranges labelled constant / ID / counter / length / value / checksum / high-entropy, with confidence.
  - `DraftSignature`: fingerprint fields for C18 (docs/04 §7.6), flex-style.
  - `VerificationReport`: held-out CRC pass rate, messages used, ambiguities.
- **Control:** a minimum corpus size, a search budget (CRC widths, LFSR list), and interactive hints from the C39 inspector (e.g. "these bits are the ID").

## Methods
- **Line code** from edge-timing and run-length statistics: NRZ, NRZI, Manchester, differential, PWM, PPM. rtl_433 `-A` timing histograms are the reference (docs/04 §7.1, §7.5).
- **Preamble/sync** (docs/04 §7.3):
  - A `1010…` preamble gives a tone at R_s/2.
  - Search known sync words by sliding correlation in both polarities and all PSK rotations: CC1101 `0xD391`, POCSAG `0x7CD215D8`, AIS `0x7E`, DMR/P25 48-bit patterns.
  - **Blind discovery:** align bursts on the preamble end, take the longest common bit substring, and mark zero-entropy positions as preamble/sync/fixed header.
- **Whitening** (docs/04 §7.4): try PN9 `x^9+x^5+1` (CC1101, 802.15.4g), BLE `x^7+x^4+1` seeded by channel, and 802.11 `x^7+x^4+1`. Score by entropy drop or CRC validity.
- **CRC reverse engineering** (docs/04 §7.4):
  - XOR two same-length valid messages to cancel init/xorout, which constrains the polynomial.
  - Brute-force widths 8/16/24/32 with CRC RevEng.
  - Also try simple sums and XOR (cheap 433 MHz sensors).
- **FEC detection:** rank deficiency of sliding n-bit block matrices. Families: K=7 (171,133) convolutional, RS(255,223), BCH(31,21), Golay (docs/04 §7.4).
- **Field inference:**
  - Per-bit-position entropy across aligned messages.
  - Correlate candidate fields with side info to find counters, IDs and values (docs/04 §7.7).
  - URH WOOT'19 rule-based inference of length, address, sequence number and checksum (docs/04 §7.5).
- **Loop** (docs/04 §7.7):
  1. Cluster (DBSCAN on normalised features).
  2. Slice to bits.
  3. Align and compute entropy.
  4. Correlate with side info.
  5. Identify CRC/whitening.
  6. Emit a draft and verify on held-out bursts by CRC pass rate.

## Platform constraints
- Low, bursty CPU. Offline-style jobs are fine off the real-time path; Python is acceptable here since it is analysis, not streaming (docs/06 §2).
- Corpus collection is the real constraint. Blind RE is a multi-message problem (docs/04 §7.5), and the single half-duplex window plus C04 scheduling decide how many bursts are captured. Long dwell on one ISM band starves survey.
- Storage is modest: bits and snippets, not continuous IQ.

## Prior art and reuse
- **URH:** field inference, decodings (Manchester, whitening, inversion), protocol view. GPLv3, archived 2026; "ideas directly reusable but need a maintained home" (docs/03 §3.4, §6).
- **CRC RevEng, delsum:** CRC/Fletcher/modsum search. Wrap as a subprocess or port the algorithms. Licence: check.
- **rtl_433 flex decoder (`-X`):** a target format for draft specs; `-A` analyzer (docs/04 §7.5). Licence: check.
- **inspectrum:** manual cursor-based symbol extraction, a UX reference for C39 (docs/03 §3.4).
- **Netzob:** message-format and state-machine inference (RESEARCH-006). Licence: check; not covered in docs/03.

## Pitfalls
- **Too few or too similar messages:** constant payloads look fixed, and CRC search needs variety. Report "insufficient corpus".
- **False CRC matches** from wide brute force over few messages. Always verify on held-out bursts.
- Bit slips, inversions and uncertain preamble length misalign the corpus. Realign on sync.
- Mixed emitters in one cluster blur entropy maps. Length fields that are whitened or FEC-coded. Partial-range checksums (delsum).
- **Encrypted or rolling-code payloads look high-entropy.** Label them and stop: metadata only, no cryptanalysis of others' devices (docs/04 §1.3).

## Testing
- **Synthetic frame generator:** random preamble length, sync word, optional PN9 whitening, a length field, counter and ID fields, and CRC-8/16/32 with random parameters, passed through C20-style bit errors and slips. Assert:
  - exact recovery of sync, LFSR and CRC params;
  - field boundaries within ±1 bit;
  - the minimum message count needed;
  - negative control: random bits give no CRC model above a pass-rate threshold.
- **SigMF fixtures:**
  - ISM sensors decodable by rtl_433. Hide the decoder, infer, then compare the field map and CRC against rtl_433 source and output.
  - AIS/ADS-B frames: known CRCs.
  - RDS blocks: known 10-bit checkword and offset words.
  - The user's own 433 MHz remotes or sensors.
- **Live hardware:** none required beyond gathering corpora.

## Example use cases
Provisional until docs/06 §3 mapping:
- RESEARCH-001 — Blind ISM device RE with URH
- RESEARCH-002 — rtl_433 flex decoder
- RESEARCH-006 — Protocol inference with Netzob
- RESEARCH-008 — Identify line coding
- RESEARCH-009 — CRC reverse engineering (differential technique)
- RESEARCH-010 — CRC RevEng
- RESEARCH-011 — delsum checksum toolbox
- RESEARCH-012 — Whitening/scrambler identification
- RESEARCH-013 — FEC identification
- AWARE-036 — Unknown burst reverse-engineering triage

## Open questions
- **Draft spec format:** an rtl_433 flex superset, or a new signature schema shared with C18 and runnable by C22?
- **FEC decoding ownership:** docs/04 §7.1 lists FEC decoding (Viterbi/RS/BCH) in the pipeline, but docs/06 gives C21 only "FEC structure detection". C22 pipelines, or a shared library?
- Line-code identification is listed in C21, but slicing happens in C20. Where is the boundary?
- Auto-trigger at N bursts vs inspector-only; how C21 asks C04 for more dwell.
- Port URH inference (GPLv3, archived) or reimplement from the WOOT papers?

## Reading list
1. `docs/04 §7.7 "Blind reverse engineering of unknown protocols (practical loop)"`
2. `docs/04 §7.4 "CRC, scrambling, FEC identification"`
3. `docs/04 §7.3 "Preamble and sync-word detection"`
4. `docs/04 §7.5 "How existing tools approach it"`
5. `docs/04 §7.6 "Protocol fingerprinting"`
6. `docs/03 §3.4 "Protocol reverse engineering and signal inspection"`
