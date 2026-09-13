# C11 · channelizer
> Layer B — Sense · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C01, C03 (requests from C04, C09, C22, C23) · Used by: C13, C14, C16, C19, C20, C22, C23, C25, C34

## Purpose
Splits the ≤20 MHz dwell window into many narrowband streams at once: a 2× oversampled polyphase filter bank (PFB) on fixed rasters (12.5/25/100 kHz) plus on-demand DDCs. It lets one half-duplex radio demodulate, decode and record every active signal in the window in parallel (trunking, dense ISM). It serves workflow steps 5–7. **C11 owns the decimated narrowband "zoom" stream** (the 25 Hz-bin / ~800k-point case), which C07 then FFTs (docs/06 §5). **C23 trunking-follow depends on C11 + C12** — control-channel hunting uses the channelizer and occupancy (docs/06 §5 / §2.1).

## Interface
- **In:** continuous IQ window + ring buffer from C03 (20 Msps complex, sample-indexed timestamps, provenance).
- **Control (provisional `ChannelRequest`):** `{f_centre, bw_hz, out_rate, purpose, requester, priority, lifetime}`; raster config `{spacing, oversample=2, prototype taps}`; release on silence/timeout.
- **Out (provisional `ChannelStream`):** complex baseband at ~2× channel spacing (PFB) or the requested rate (DDC); `t0` sample index aligned to the parent window, `f_offset`, filter spec, gain/clip provenance, gap/overflow markers.
- **Derived sizing:** 20 MHz / 12.5 kHz ≈ 1600 channels × 25 kSps = 40 Msps total, twice the input. Only materialize active channels.

## Methods
- **Per-signal DDC:** NCO mix x[n]·e^(−j2πf₀n/fs), CIC or halfband cascade, FIR cleanup. O(N) per signal; best for <~10 signals of differing bandwidths. `docs/04 §3.7 "Channelization: DDC and polyphase filter banks"`.
- **PFB:** M uniform channels from one prototype lowpass of length ≈ K·M; cost O(KM + M·log M) per input block, independent of active channel count. Wins for dense rasters. Use **2× oversampled** so edge-straddling signals survive, and synthesize adjacent channels to recombine wider signals. `docs/04 §3.7`.
- **Hybrid (recommended):** coarse PFB (25–100 kHz) for surveillance plus narrow DDCs spawned per detection at exact f_c/BW. `docs/04 §3.7`.
- **Trunking layout:** PFB on a 12.5 kHz raster, 2× oversampled → CC hunter → grant handler allocates a demod per channel. The PFB is the dominant fixed cost. `docs/04 §8.4 "Architecture for trunking support"`.
- **Stopband:** a PFB gives far better stopband than plain FFT bins, which is essential for weak narrowband signals next to strong ones. The MazinLab OPFB reports images below −60 dB. `docs/02 §4 "FPGA roles in an exploration device"`.
- Prototype defaults (K, window, ripple) aren't in the docs; spike.

## Platform constraints
- Input ceiling: 20 Msps 8-bit over USB 2.0 (~40 MB/s); usable ~15–18 MHz after filter skirts; DC spike at the centre on HackRF One. Place rasters away from DC, or offset-tune. `docs/01 §7.3 "HackRF as a front end for Pi 5 / Orin Nano (± FPGA)"`.
- An 800 MHz trunked system spans ~18 MHz of 851–869 MHz; some 700/800 MHz systems exceed 20 MHz (unverified). `docs/04 §8.2 "Why wideband capture helps"`, `docs/01 §7.3`.
- Compute: a PFB with 256–4096 channels costs roughly FFT + M-tap filter per frame. CPU is OK at 20 MHz; Orin GPU is comfortable at 56–100 MHz (estimates). `docs/02 §3.2 "DSP compute: order-of-magnitude feasibility"`.
- The Orin's unified memory avoids PCIe copies for GPU PFB. `docs/02 §3.3 "Platform comparison"`.
- HackRF Pro FPGA (5,280 LUTs) can't host a multi-channel channelizer. `docs/01 §1.7 "HackRF Pro (codename "Praline")"`.

## Prior art and reuse
- **liquid-dsp:** filters, NCO, resamplers for DDCs; C, no dependencies, active. Licence: check. `docs/03 §1.4 "DSP libraries"`.
- **CuPy `cupyx.scipy.signal`** (ex-cuSignal) or a custom CUDA PFB. Licence: check. `docs/03 §1.7 "GPU / ML stacks"`.
- **Trunk Recorder:** capture-everything; a recorder per grant attached to a slice of the wideband stream. Maintainers note several smaller SDRs are often better than one wide one (CPU, dynamic range). Licence: check. `docs/03 §3.5 "Trunking and digital voice (the "CB/trunk complaint")"`.
- **SDRangel device sets:** channel plugins at offsets inside one baseband; SigMF sink per slice. Licence: check. `docs/03 §2.2 "SDRangel — the most engineering-grade open-source receiver"`.
- **MazinLab OPFB, RFNoC channelizer:** FPGA references for a future front end. `docs/02 §4`.

## Pitfalls
- **Critically sampled PFB loses signals at channel edges**; use 2× oversampling.
- **Weak prototype stopband** leaks strong channels into neighbours, causing false detections downstream.
- **Strong in-window signal sets dynamic range** (8-bit); the channelizer cannot recover what the ADC clipped. Propagate clip flags per channel. `docs/02 §1.2 "ADC bit depth, SNR, ENOB, SFDR"`.
- **Raster offset:** a crystal ppm error shifts every channel. Apply the C05 frequency correction before assigning rasters.
- **Time alignment:** PFB and DDC group delays differ; compensate timestamps or C25/C10 timing drifts.
- **Retunes** (C04) invalidate all streams; signal discontinuity. A slow consumer must not stall the window; drop and mark gaps per channel.
- **Legal:** the channelizer is content-neutral, but routing policy must stop cellular and common-carrier paging *content* and encrypted-traffic decryption from reaching demods/outputs. Metadata only. `docs/04 §1.3 "Legal considerations (US; not legal advice)"`.

## Testing
- **Synthetic:** tone comb on a 12.5 kHz raster ± half-channel offsets (ripple, adjacent-channel rejection, edge-straddle recovery); a +60 dB carrier next to a weak one (leakage); a 20 MHz chirp (boundary continuity). PFB vs reference DDC SNR loss < 0.5 dB (proposed).
- **Timing:** impulse at known sample index; per-channel t0 within 1 output sample (proposed).
- **Throughput:** 1600-channel PFB real-time at 20 Msps on Orin GPU and CPU; latency, GPU%, W per power mode.
- **SigMF fixtures (HackRF One):** local P25/DMR control + voice (metadata/unencrypted only); 433/915 MHz ISM with simultaneous sensors; 118–137 MHz airband; 162.4–162.55 MHz NOAA Weather Radio.
- **Live only:** sustained multi-decoder load under thermal limits.

## Example use cases
Regenerated from `use-cases.yaml`:
- SIGNAL-003 — (parallel narrowband decode)
- SIGNAL-004 — (parallel narrowband decode)
- SIGNAL-006 — (parallel narrowband decode)
- SIGNAL-007 — (parallel narrowband decode)
- SIGNAL-023 — Iridium bursts & ring alerts
- SIGNAL-041 — (dense-band channelization)
- SIGNAL-042 — (dense-band channelization)
- SIGNAL-044 — MPT1327 trunked fleets
- SIGNAL-049 — (parallel narrowband decode)
- SIGNAL-052 — rtl_433 long tail
- Also feeds trunking (SIGNAL-080..084, via C23).

## Open questions
- **C11 upstream (resolved, docs/06 §2.1):** C11 depends on C03 and receives requests from C04/C09/C22/C23.
- GPU (CUDA/CuPy) vs CPU (liquid-dsp/VOLK) PFB default; tied to the pipeline-framework ADR and the "no rebuild to change pipelines" requirement (add/remove channels at runtime).
- PFB prototype parameters and which rasters run by default (energy/power budget in low-power mode).
- **Zoom-stream ownership (resolved, docs/06 §5):** C11 owns the decimated zoom stream; C07 FFTs it.
- Where does routing/legal policy live: C22 registry or a channel-request gate? (Restricted-content gating is owned by C24 per docs/06 §5.)

## Reading list
1. `docs/04 §3.7 "Channelization: DDC and polyphase filter banks"`
2. `docs/04 §8.4 "Architecture for trunking support"`
3. `docs/02 §3.2 "DSP compute: order-of-magnitude feasibility"`
4. `docs/02 §4 "FPGA roles in an exploration device"`
5. `docs/03 §3.5 "Trunking and digital voice (the "CB/trunk complaint")"`
6. `docs/04 §1.3 "Legal considerations (US; not legal advice)"`
