# C20 · digital-demod
> Layer D — Demodulate & decode · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C11, C13, C14, C15 · Used by: C21, C22, C23, C24, C18, C19, C39

## Purpose
Blindly converts a channelized digital emission into soft symbols and hard bits, using parameters estimated upstream (family, symbol rate, deviation, CFO) instead of per-protocol configuration. It is the gateway from "we know what shape it is" to "we have bits". **C20 owns demodulation of OFDM, CSS and radar-pulse waveforms; C13/C14/C16 only *estimate* their parameters — the split is estimate (C13/C14/C16) vs recover (C19/C20)** (docs/06 §5). Conventional (non-trunked) digital voice (P25/DMR/NXDN) is C20 + C22; C23 is only the trunking control/grant logic (docs/06 §5). Serves workflow step 6 (blind symbol and bit recovery, including unknown signals) and supplies step 7 via C22/C24. Layer D is not a chain — C22 does not require C21 (docs/06 §2.1).

## Interface
- **In:**
  - `ChannelStream` (provisional) from C11, or a `BurstSnippet` from C09/C25: complex baseband, sample rate, sample-time index, provenance.
  - `ParameterSet` from C13: CFO, OBW, SNR.
  - `SymbolParams` from C14: symbol rate, levels/order, deviation, roll-off, BT.
  - Family label from C15: OOK/ASK, 2/4-FSK/GFSK/MSK, PSK/QAM, CSS.
- **Out:**
  - `SymbolStream` (provisional): soft symbols per symbol (complex for PSK/QAM, real for FSK/ASK), sample time of each symbol, symbol-rate estimate after loop convergence.
  - `BitStream`: hard bits, polarity/rotation ambiguity flag, burst boundaries, source emission id.
  - `LockQuality`: timing-error variance, EVM or eye opening, carrier-lock flag, time to lock, slip count.
  - For OOK/FSK bursts, optionally a `PulseList` of pulse/gap or mark/space durations, rtl_433-style (docs/04 §7.5).
- **Control:** burst mode vs continuous mode, loop bandwidth override, and a "try alternatives" budget (e.g. both FSK polarities, all PSK rotations).

## Methods
- **Per-family demod** (docs/04 §7.1):
  - OOK/ASK: `|x|`, then an adaptive two-level slicer (k-means or Otsu per burst).
  - FSK: quadrature discriminator `f[n] = arg(x[n]·x*[n−1])`, then a 2- or 4-level slicer. No carrier lock needed, so FSK is the easiest blind family (docs/04 §7.2).
  - PSK/QAM: RRC matched filter, then timing recovery, then Costas (BPSK/QPSK), a 4th-power loop (QAM) or decision-directed PLL.
  - CSS: dechirp with the conjugate base chirp, FFT, symbol = argmax bin.
- **Timing recovery** (docs/04 §7.2):
  - **Gardner**, 2 samples/symbol, carrier-phase independent. Recommended default for blind PSK/QAM.
  - Mueller & Müller: 1 sps, decision-directed, ≈20–30 symbols to lock, needs carrier phase.
  - Early–late or zero-crossing for FSK/OOK.
  - Polyphase filter-bank clock sync (N_f ≈ 32) for accuracy.
  - Loop: 2nd-order PI, `B_nT ≈ 0.005–0.02`, damping ≈0.707. Wider for short bursts. For bursts under ~50 symbols use a feedforward Oerder–Meyr estimator.
- **Line codes after slicing:** NRZ, NRZI, Manchester, differential, PWM, PPM (docs/04 §7.1). Inference proper belongs to C21.
- **Pulse abstraction:** the rtl_433 slicer library (OOK PCM/RZ, PPM, PWM, Manchester, differential Manchester, PIWM, FSK variants) plus a timing histogram covers most sub-GHz ISM cheaply (docs/04 §7.5, §12 #10).
- **Ambiguity:** resolve polarity and rotation downstream with a sync-word search in all polarities and rotations (docs/04 §7.3).
- **Simulcast P25 CQPSK/LSM:** needs coherent QPSK with equalization, not a C4FM discriminator (docs/04 §8.1).

## Platform constraints
- Low–medium cost per channel (docs/06 §2). Robustness is "medium (burst length, SNR)" (docs/04 §12 #11).
- All channels come from one ≤20 MHz half-duplex window; bursts outside it are missed (docs/04 §3.8).
- **BLE advertising (1 Msps GFSK, fixed 2402/2426/2480 MHz channels) is a good 2.4 GHz fit for this family demod path** — plain GFSK, no carrier lock needed, well inside the window — unlike 2.4 GHz Wi-Fi (OFDM, C16, mostly out of reach at 20 Msps) or classic Bluetooth (79-channel hopping, C10, needs ~80 MHz). Capability statement, unverified until measured — `docs/02 §2.3`.
- HackRF One has no TCXO (docs/01 §1.2), so CFO and sample-clock drift must be tracked, not assumed. CFO comes from C13/C05.
- 8-bit ADC with ~6 bits effective (docs/01 §1.3). Strong neighbours cut SNR and degrade high-order QAM first.
- Snippets can be processed off the real-time path. Continuous streams (control channels, AIS) need a native real-time implementation. Python is research only.

## Prior art and reuse
- **liquid-dsp:** modems, framing, NCO/PLL, AGC; dependency-free C, very active (docs/03 §1.4). Licence: check.
- **Suscan/SigDigger:** generic ASK/FSK/PSK demodulators, blind baud estimation, symbol recording, UDP broadcast of symbols (docs/03 §3.4). Licence: check. Single maintainer.
- **URH:** automatic ASK/FSK/PSK parameter detection heuristics. GPLv3, archived 2026 (docs/03 §3.4).
- **rtl_433:** pulse detector plus slicers plus `-A` analyzer (docs/04 §7.5). Licence: check.
- **GNU Radio `symbol_sync` / `pfb_clock_sync`:** the PFB reference (docs/04 §7.2). GR3 blocks are GPLv3 (docs/03 §1.2).
- **SatDump:** pipelines can start from soft symbols, so emitting soft symbols lets C22 reuse SatDump deframers (docs/04 §7.5).

## Pitfalls
- Clock slips from sample-clock drift; loops not converging on short packets (use feedforward).
- Costas false lock at 90°/180°; FSK inversion. Never assume polarity.
- A wrong C14 symbol rate (weak at small roll-off; docs/04 §12 #7) yields garbage bits with "lock".
- OOK thresholds dragged by inter-pulse noise; the DC spike biasing the discriminator; GFSK ISI; 4FSK levels from short data.
- Lock metrics look good on noise. Require downstream sync or CRC confirmation.

## Testing
- **Synthetic** (Python/TorchSig generators; docs/03 §4.1): random bits into OOK, 2/4-FSK, GFSK, BPSK, QPSK, 16QAM and CSS with RRC, CFO, sample-clock offset (ppm), timing phase and AWGN sweeps. Assert:
  - BER within a tolerance of the theoretical curve (tolerance is an estimate to set in the test ADR);
  - time to lock in symbols (M&M reference ≈20–30);
  - zero slips over N symbols at a given SNR;
  - polarity/rotation flags correct.
- **SigMF fixtures** (HackRF-capturable):
  - ISM sensors: compare bits with rtl_433's decode of the same IQ.
  - AIS: HDLC flag 0x7E and CRC pass rate.
  - ADS-B: Mode S preamble and CRC.
  - POCSAG: sync word `0x7CD215D8` hit count and frame timing only, metadata only (docs/04 §1.3).
  - RDS BPSK via C19 audio.
- **Live hardware:** long-duration drift, strong-neighbour desense, P25 simulcast.

## Example use cases
Regenerated from `use-cases.yaml`:
- RESEARCH-001 — Blind ISM device RE with URH
- RESEARCH-017 — (digital demod primary)
- RESEARCH-043 — (digital demod primary)
- RESEARCH-044 — (digital demod primary)
- RESEARCH-046 — (digital demod primary)
- RESEARCH-064 — (digital demod primary)
- SIGNAL-016 — (digital bit recovery)
- SIGNAL-024 — (digital bit recovery)
- SIGNAL-054 — (digital bit recovery)
- SIGNAL-060 — (digital bit recovery)

## Open questions
- Where does the rtl_433-style pulse abstraction live: C20 (demod), C21 (line-code inference) or only inside the rtl_433 plugin (C22)?
- Soft-symbol format and scaling (float32, int8, LLRs) shared with C22/SatDump (doc 07).
- Burst API vs streaming API; whether snippet demod can batch on the GPU.
- **CQPSK/LSM equalizer and OFDM demod (resolved, docs/06 §5).** C20 owns OFDM/CSS/pulse demodulation (C16 only estimates parameters); conventional digital voice is C20+C22. C23 is only trunking control/grant logic. The CQPSK/LSM equalizer belongs to C20.
- Library choice (liquid-dsp vs Suscan vs a framework's blocks) and licences: Phase 3 ledger.

## Reading list
1. `docs/04 §7.1 "Pipeline overview"`
2. `docs/04 §7.2 "Clock (symbol timing) recovery"`
3. `docs/04 §7.5 "How existing tools approach it"`
4. `docs/04 §7.3 "Preamble and sync-word detection"`
5. `docs/03 §3.4 "Protocol reverse engineering and signal inspection"` (URH, SigDigger)
6. `docs/04 §4.4 "Symbol-rate estimation"` and `§4.6 "FSK deviation and modulation index"` (input quality)
