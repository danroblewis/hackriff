# C23 · trunking-follow
> Layer D — Demodulate & decode · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C11, C20, C19, C22, C12, C17, C04 · Used by: C24, C25, C27, C30, C12

## Purpose
Finds trunked land-mobile systems automatically, decodes control channels continuously, and follows voice grants onto channelizer outputs inside the dwell window. It produces a system model, call records, and audio for **unencrypted** calls only; encryption is flagged as metadata. It removes the manual control-channel hunt even Trunk Recorder requires (docs/03 §3.5). Serves workflow steps 5–7 for LMR.

Edges (docs/06 §5/§2.1): C23 depends on **C11 (channelizer) + C12 (occupancy)** — continuous-duty control-channel detection uses the occupancy/FCO baseline — not on C21; docs/06 §2.1 no longer draws a linear C22→C23 chain. Conventional (non-trunked) digital voice is owned by C20 + C22; C23 is only the trunking control/grant logic.

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
  - **A call still keyed when the dwell ends is truncated, never closed at the window edge** (T-308, docs/07 §2.29): `t_end` stays NULL and `observed_until` records where watching stopped, so the duration reads as a lower bound. A later pass continues it only across a gap ≤ the silence timeout (90 ms) — the duty cycle's gap is ~9.5 s, so ordinarily it does not, and the row stays truncated.
- **Encryption check before the vocoder** (docs/04 §8.3):
  - P25 ALGID 0x80 = clear; 0x81 DES-OFB, 0x84 AES-256, 0xAA ADP/RC4.
  - The ALGID and key id live in each call's LDU2 encryption sync (talkgroup/source in its LDU1 link control). `hk_detect::trunk::ldu` (T-849) reads both off a followed FDMA channel — status symbols, BCH(63,16) NID, Hamming(10,6,3) hexbits, RS(24,12)/RS(24,16) over GF(64), IMBE skipped — and the follower records them on the call's `call-start` event (`detail.voice_frames`). Recalled, not verified against a real capture. `CallHeader::fold` (T-330) folds a call's LDU2 ALGIDs into its encryption state: the header replaces an `unknown` grant and names the algorithm behind an `encrypted` one, but a clear header never walks back an encrypted grant (kept encrypted, reason `algid-contradicts-grant`). Only clear-by-own-ALGID earns a `VoicePermit`, still only via `VoicePermit::open`.
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
Regenerated from `use-cases.yaml` (now includes the new proposed SIGNAL-080..086 trunking IDs):
- AWARE-067 — Public-safety trunking load index (metadata only)
- SIGNAL-044 — MPT1327 trunked fleets
- SIGNAL-080 — (trunking-follow primary)
- SIGNAL-081 — (trunking-follow primary)
- SIGNAL-082 — (trunking-follow primary)
- SIGNAL-083 — (trunking-follow primary)
- SIGNAL-084 — (trunking-follow primary)
- SIGNAL-085 — (trunking-follow primary)
- SIGNAL-086 — (trunking-follow primary)

## Open questions
- Vocoder licensing: mbelib vs hardware AMBE vs metadata-only default (Phase 3 ledger).
- Build vs wrap: native CC decoders on C11 outputs, or Trunk Recorder/OP25/SDRTrunk subprocesses fed channel IQ?
- **Conventional digital voice (resolved, docs/06 §5):** owned by C20 + C22 (via dsd-fme); C23 is only trunking control/grant.
- CQPSK equalizer owner (C20 vs C23); C04 pre-emption policy.
- Call-audio streaming/upload policy (C24 gating, docs/06 §5); **own-system key handling is owned by C22** (own-key decryption, provisional — docs/06 §5).
- **Dependency (resolved, docs/06 §2.1/§5):** C23 depends on C11 + C12, not on C21; the old linear C22→C23 edge is dropped.

## Reading list
1. `docs/04 §8.4 "Architecture for trunking support"`
2. `docs/04 §8.1 "How trunking works"`
3. `docs/04 §8.3 "Encryption status"`
4. `docs/03 §3.5 "Trunking and digital voice (the "CB/trunk complaint")"`
5. `docs/04 §8.2 "Why wideband capture helps"`
