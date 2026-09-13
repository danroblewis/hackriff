# C23 · trunking-follow
> Layer D — Demodulate & decode · Status: draft (taxonomy draft 2026-09-13) · Depends on: C11, C20, C19, C22, C12, C17, C04 · Used by: C24, C25, C27, C30, C12

## Purpose
Finds trunked land-mobile systems automatically, decodes control channels continuously, and follows voice grants onto channelizer outputs inside the dwell window. It produces a system model, call records, and audio for **unencrypted** calls only; encryption is flagged as metadata. It removes the manual control-channel hunt even Trunk Recorder requires (docs/03 §3.5). Serves workflow steps 5–7 for LMR.

## Interface
- **In:**
  - C11 streams on the LMR raster (12.5 kHz, 2× oversampled PFB; docs/04 §8.4).
  - FCO and co-occurrence from C12/C10 for CC candidates.
  - Talkgroup priors from C17 (RadioReference).
  - Window hold/release with C04.
- **Out** (provisional):
  - `TrunkSystem`: protocol, system/site IDs, CC frequency, channel tables (base + spacing + TX offset), neighbour sites, talkgroups.
  - `CallRecord`: start/end, talkgroup, unit ID, channel/slot, frequency, encryption flag with ALGID/Key ID, vocoder, emission id.
  - `CallAudio` for unencrypted calls, to C25/C24.
  - `GrantEvent` stream for metadata-only load indices.
  - Optional CC IQ for audit.
- **Control:** per-system enable; voice policy (metadata-only / clear audio / own-system keys); max concurrent calls; talkgroup filters.

## Methods
- **CC hunting** (docs/04 §8.4):
  1. Candidates: continuous 4FSK/C4FM with 100% FCO on the LMR raster.
  2. Confirm by frame sync (P25 FS `0x5575F5FF77FF`, DMR sync, SmartNet 3600 bps; docs/04 §7.3) plus valid CRCs.
  3. Inter-channel correlation (docs/04 §2 #14) groups system frequencies before decode.
- **CC decode** (docs/04 §8.1):
  - P25 Phase 1 TSBK at 9600 bps; SmartNet 3600; EDACS 9600; DMR Tier III/Capacity Max; NXDN Type-C.
  - `IDEN_UP` maps 16-bit channel numbers to frequencies. Use grant updates for late entry.
  - No dedicated CC: Capacity Plus rest channel, NXDN Type-D, LTR subaudible.
- **Grant following:** allocate a voice demod on the channel or TDMA slot: C4FM/CQPSK/H-DQPSK/4FSK via C20, analog FM via C19 for analog SmartNet/EDACS. End on a silence timeout (docs/03 §3.5).
- **Encryption check before the vocoder** (docs/04 §8.3):
  - P25 ALGID 0x80 = clear; 0x81 DES-OFB, 0x84 AES-256, 0xAA ADP/RC4.
  - Grant service-options bit; DMR privacy indicators in LC/PI headers.
  - Encrypted calls are labelled and skipped. **Never** decrypt others' traffic; own system with own keys only.
- **Simulcast:** CQPSK/LSM needs coherent QPSK with equalization (docs/04 §8.1).
- **Vocoders:** IMBE (P25 Phase 1), AMBE+2 (Phase 2, DMR, NXDN). mbelib exists; DVSI IP needs review; hardware AMBE is the licensed path (docs/04 §8.4).

## Platform constraints
- **Compute** (docs/04 §8.4): the CC decoder is cheap; each voice channel is ~a 4FSK demod plus vocoder, "a few % of a modern ARM core". The 20 MHz PFB is the dominant fixed cost and a GPU fit (docs/02 §3.2).
- **Span:** ≤20 MHz per window (usable ~15–18 MHz; docs/01 §7.3). An 800 MHz system over ~18 MHz of 851–869 MHz is borderline (docs/04 §8.2); some 700/800 MHz systems exceed 20 MHz [unverified]. Log "grant outside window".
- **Half-duplex, single window:** following parks the radio and starves survey. C04 needs pre-emption rules.
- **Dynamic range:** 8-bit, no preselector; nearby sites desense (docs/01 §1.3). Trunk Recorder prefers several narrow SDRs to one wide one (docs/04 §8.2), which a single HackRF can't do.

## Prior art and reuse
- **Trunk Recorder:** capture-everything (continuous CC, a recorder per grant, silence timeout, uploads); P25/SmartNet/trunked DMR with LCN→frequency mapping; `conventionalSIGMF` input; active (docs/03 §3.5). Licence: check. Subprocess candidate.
- **SDRTrunk** (Java): P25 1/2, DMR, LTR, MPT1327; slow releases (docs/03 §3.5). Licence: check. A protocol-logic reference.
- **OP25 (boatbod):** GNU Radio P25/DMR/SmartNet, active. GR3 blocks are GPLv3 (docs/03 §1.2).
- **dsd-fme:** digital voice plus trunking, active. Licence: check.
- **Mayhem TETRA RX:** CC metadata only, no audio. A scoping precedent (docs/01 §3.3).
- **GPL isolation:** run GPL stacks as processes behind the C22 contract. The licence decision is deferred.

## Pitfalls
- Simulcast LSM decoded as C4FM: high BER, missed grants.
- False CCs: continuous data emitters pass the FCO test. Require sync plus CRC.
- CC dropouts on overload; stale IDEN tables mapping to wrong frequencies.
- TDMA slot mix-ups (DMR/Phase 2).
- Incident load exceeding CPU: prioritise talkgroups, shed audio, keep records.
- Late entry without a header: encryption status should default to "unknown", not "clear".
- Vocoder crashes and recording backpressure.
- **Legal:** publicly streaming call audio risks 47 USC 605 divulging; state mobile-scanner laws (docs/04 §1.3). Metadata-only indices are the safe sharing default.

## Testing
- **Offline fixtures:**
  - Short 20 Msps SigMF clips of a local P25 or DMR system (40 MB/s, so minutes only).
  - CC-only narrow recordings for decoder unit tests.
  - Compare against Trunk Recorder/SDRTrunk on the same IQ.
- **Assertions:**
  - CC found with zero config;
  - IDEN parsed;
  - grant → frequency correct;
  - call boundaries within tolerance;
  - encrypted calls flagged with ALGID and producing no audio;
  - grant counts match the reference.
- **Synthetic:** simulated continuous 4FSK among bursty NBFM for the CC hunter; sync false-alarm rate on noise.
- **Live hardware:** simulcast sites, systems wider than the window, long following under C04 pre-emption, Jetson CPU under many calls.

## Example use cases
Provisional until docs/06 §3 mapping (the catalogue has few trunking entries):
- AWARE-067 — Public-safety trunking load index (metadata only)
- SIGNAL-044 — MPT1327 trunked fleets
- RESEARCH-019 — TETRA:BURST (encryption awareness only; no implementation)

## Open questions
- Vocoder licensing: mbelib vs hardware AMBE vs metadata-only default (Phase 3 ledger).
- Build vs wrap: native CC decoders on C11 outputs, or Trunk Recorder/OP25/SDRTrunk subprocesses fed channel IQ?
- The catalogue has ~2 trunking cases, although docs/04 §12 #13 calls trunking "most-requested". Add P25/DMR/NXDN discovery cases?
- **Conventional digital voice** (DMR/P25/NXDN via dsd-fme) has no owner in docs/06.
- CQPSK equalizer owner (C20 vs C23); C04 pre-emption policy.
- Call-audio streaming/upload policy (C24); own-system key handling (no owner).
- docs/06 §2.1 draws C22→C23 linearly. C23 really depends on C11/C12, not C21.

## Reading list
1. `docs/04 §8.4 "Architecture for trunking support"`
2. `docs/04 §8.1 "How trunking works"`
3. `docs/04 §8.3 "Encryption status"`
4. `docs/03 §3.5 "Trunking and digital voice (the "CB/trunk complaint")"`
5. `docs/04 §8.2 "Why wideband capture helps"`
6. `docs/04 §1.3 "Legal considerations (US; not legal advice)"`
