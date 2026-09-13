# C03 · dwell-capture
> Layer A — Acquire · Status: draft (taxonomy draft 2026-09-13) · Depends on: C01, C04, C05, C06 · Used by: C05, C07, C11, C25, C33, C34, C36, C39

## Purpose
Parks the radio on one real-time window of up to ~20 MHz and streams it into a RAM ring buffer with a sample-accurate time index. A detection can then pull **pre-trigger** history into a recording. It finds *what* a signal is, with near-100% intercept inside the window versus under 1% in a sweep. It is part of the substrate every live use case needs, and serves workflow steps 1, 5 and 6.

## Interface
- **Input (provisional `TuneRequest`, via C04):** centre, sample rate (8–20 Msps), filter, gain profile, lease duration, priority, requester (scheduler, user pin, decoder).
- **Outputs:**
  - Live `IQWindow` to C07/C11: views, fs, centre, usable-band mask excluding DC and edges.
  - `RingBuffer.extract(t0, t1)` by sample index or UTC, returning samples + provenance.
  - `DwellRecord`: start/end, centre, rate, gain history, clip/discontinuity events.
- **Sizes:**
  - 20 Msps int8 = 40 MB/s, so 30 s pre-trigger = 1.2 GB (docs/06 C03 row).
  - float32 complex would be 160 MB/s (derived), so keep the ring as raw int8.
  - Full-rate recording is 144 GB/h (docs/02 §3.1 "Throughput math"), so recordings are snippets.
- **Control:** retune (segments buffer), extend lease, pre/post-trigger lengths, pin (C23 trunking, C34 passes).

## Methods
- **Ring:** circular buffer of raw C01 blocks with a monotonic sample-index → UTC map. Segment on retune, gain change or discontinuity, so an extraction never silently spans two states.
- **Offset tuning:** tune ¼ IBW away and DDC back, plus a DC-blocking IIR. Discard ≥10% band edges (docs/04 §10.3 "Spur identification and removal"). The usable window is ~15–18 MHz of 20 (docs/01 §7.3).
- **Burst snippets:** ±20% padding around burst records (docs/04 §3.6 "Burst detection and segmentation").
- **Dwell length:** proportional to expected burst intervals, seconds to minutes (docs/04 §3.8 "Sweep-based survey vs. real-time IBW"). Complete capture needs revisit ≤ ½ the minimum on/off time (docs/04 §3.9).
- **Capture everything, decide later:** recorders attach to slices of the buffered wideband stream (docs/03 §5.2 "Best UX ideas that already exist (steal these)").
- **Narrow targets:** run the ADC at ≥8 Msps and decimate on the host (docs/01 §1.2 "Specifications").

## Platform constraints
- ≤20 MHz contiguous 8-bit; no gap-free coverage beyond one window (docs/01 §7.3 "HackRF as a front end for Pi 5 / Orin Nano (± FPGA)").
- USB 2.0 at its practical limit at 20 Msps (docs/01 §1.4 "USB 2.0 throughput ceiling").
- Orin Nano Super: 8 GB unified memory shared with OS and GPU models (docs/02 §3.3 "Platform comparison"). Ring size is a budget decision.
- 4096-bin FFT at ~4.9k/s plus a channelizer in-window is feasible (estimate; docs/02 §3.4 "What's realistic at 20 / 56 / 100+ MHz").
- 8-bit: a strong in-window signal blocks weak ones (docs/01 §1.3 "Noise figure, dynamic range, and overload").
- Pro only: 16-bit ENOB 9–11 at ≤2.5 Msps output for weak-signal dwell (docs/01 §7.3).

## Prior art and reuse
- **Mayhem Signal Hunter:** 8 ms pre-trigger ring, energy trigger (docs/01 §3.4 "Spectrum and "exploration" apps and their limits").
- **SDRangel SigMF File Sink:** squelch trigger, pre-trigger seconds, post-trigger hold, multiple slices per baseband (docs/03 §2.2). Licence: check.
- **Trunk Recorder:** per-grant recorders on the wideband stream (docs/03 §3.5). Licence: check.
- **Maia SDR:** SigMF recording capped by RAM at 400 MiB (docs/03 §2.4 "Web-based and embedded receivers").
- **FPGA triggers with BRAM/DDR pre-trigger buffers:** the future path via HackRF Pro trigger connectors (docs/02 §4 "FPGA roles in an exploration device").

## Pitfalls
- **Misaligned pre-trigger:** undetected USB drops shift the index→UTC map. Flag and never interpolate over gaps.
- **DC spike:** a target at the tuned centre sits on the One's spike. Always offset-tune.
- **IQ image:** a strong in-window signal mirrors at −f, typically −25…−40 dB. Pass image-candidate flags downstream (docs/04 §10.3).
- **Unified memory:** a ring grown for long pre-trigger starves GPU inference. Enforce a hard cap.
- **Starved survey:** long pinned leases (trunking, passes) starve discovery. C04 must see lease cost.
- **Gain changes inside an extraction** alter apparent power. Segment on them.
- **Legal:** buffering is content-neutral. Extraction for recording or streaming must respect cellular/paging content and §605 rules (docs/04 §1.3 "Legal considerations (US; not legal advice)").

## Testing
- **Synthetic:** a sample-counter stream. Trigger at a known index and assert exact pre/post lengths with no gap. An injected gap produces a segment split plus flag.
- **Burst injection:** 5 ms bursts at random times in a 20 MHz window; all captured (~100% POI, docs/04 §3.8).
- **DC/image:** a centre tone plus a strong +f tone. Assert the target stays clear and the mirror is flagged.
- **Fixtures (HackRF One, 10–20 Msps):**
  - Own-device 433.92 MHz key-fob and weather-sensor bursts.
  - The 902–928 MHz ISM band.
  - Iridium 1616–1626.5 MHz (metadata).
  - ADS-B 1090 MHz as a dense short-burst reference.
- **Live:** sustained 20 Msps ring with GPU load; memory and thermals in the 15 W mode.

## Example use cases
Provisional until docs/06 §3 mapping.
- AWARE-036 — Unknown burst reverse-engineering triage
- SIGNAL-023 — Iridium bursts & ring alerts
- AWARE-034 — Wi-Fi DFS radar event logging
- AWARE-005 — "Personal privacy device" hunter
- SPACE-064 — Jupiter S-burst microstructure
- SPACE-069 — Fast radio burst hunting
- PROP-029 — Meteor-burst link experiment
- RESEARCH-003 — Bit-level dissection in inspectrum

## Open questions
- Ring depth default and NVMe spill. 1.2 GB / 30 s is a docs/06 example, not a decision. ADR with C25.
- Ring ownership vs "extend pipelines without stopping capture". A capture process owning shared memory is a candidate.
- docs/06 promises "sample-accurate timestamps" but names no method: HackRF One has no documented hardware timestamps, so this needs C06 host discipline plus a USB-latency spike.
- SPACE-069 spans 250 MHz and cannot fit one window. Should C04 support hopping dwells, with the gap recorded in `fit_note`?

## Reading list
1. docs/04 §3.8 "Sweep-based survey vs. real-time IBW"
2. docs/01 §7.3 "HackRF as a front end for Pi 5 / Orin Nano (± FPGA)"
3. docs/03 §2.2 "SDRangel — the most engineering-grade open-source receiver"
4. docs/04 §10.3 "Spur identification and removal"
5. docs/02 §3.1 "Throughput math"
6. docs/04 §3.6 "Burst detection and segmentation"
