# C24 · stream-output
> Layer D — Demodulate & decode · Status: draft (taxonomy draft 2026-09-13) · Depends on: C19, C20, C21, C22, C23, C11, C03, C25 · Used by: C22 (proposed), C39 (remote clients), external programs

## Purpose
Streams bits, soft symbols, decoded messages, audio and IQ slices to external programs over sockets or pipes, with framing, self-describing metadata and backpressure, so that other tools can turn them into something useful. It is workflow step 7 ("decoders are pluggable consumers") and the scriptable surface other tools drive. It keeps hackriff from becoming a closed decoder catalogue.

## Interface
- **Stream kinds** (provisional names):
  - `iq`: channel or window slices. SigMF datatype string (e.g. `cf32_le`, `ci8`), rate, centre.
  - `audio`: PCM rate/format. `tools/fm_rx.py` emits 48 kHz s16le mono, about 96 kB/s by arithmetic.
  - `symbols`: soft, with scaling.
  - `bits`: packed or unpacked, with burst boundaries.
  - `messages`: JSON lines from C22/C23.
  - `events`: squelch open/close, detections, lock changes.
- **Framing** (provisional): a stream header (SigMF-style `global`: datatype, sample rate, centre frequency, hackriff version, provenance), then per-block frames. Each frame has:
  - sequence number;
  - sample-time index and UTC timestamp;
  - source emission id;
  - payload length;
  - flags (gap/drop, overload, suspect IMD, encrypted, metadata-only).

  JSON lines for `messages`/`events`, binary frames for `iq`/`audio`/`symbols`/`bits`.
- **Transports** (to decide): stdout pipe for one-shot `hackriff … | consumer` use (as `fm_rx.py | ffplay`), local Unix/TCP socket for live subscribers, optional MQTT for messages.
- **Control:**
  - subscribe by emission id, stream kind or query (e.g. "all RDS messages");
  - backpressure policy per subscription: `drop-oldest` (live) or `block-with-limit` (recording-like consumers);
  - list available streams.
- **Metrics:** per-subscriber throughput, drops, lag.

## Methods
- **Self-describing streams:** SigMF metadata conventions for headers; detections and classifications as annotations (docs/03 §1.6). A consumer should be able to save a stream as a valid SigMF pair.
- **Timestamps:** carry sample-accurate time indices from C03 end to end, not arrival time (docs/06 C03). This lets consumers join streams.
- **Backpressure:** bounded ring per subscriber. Never let an external consumer stall C03/C11. Emit explicit gap frames with drop counts. Metrics are exposed to C39.
- **Content gating:** frames flagged metadata-only or encrypted never carry payload. Paging/cellular contents are excluded, and streaming content is where 47 USC 605 "divulging" risk lies (docs/04 §1.3: "recording is not the risk; publishing or streaming contents can be"). Own-traffic decrypted content is allowed.
- **Control plane:** keep API-first scripting from day one (Mayhem's USB shell lesson, docs/01 §7.1 #4; SDRangel REST automation, docs/03 §2.2), but separate it from the data plane.
- **Wide IQ:** full-window 20 Msps IQ is 40 MB/s (docs/02 §3.1). Default to channel-rate slices and offer full-rate only locally. VITA-49 packetization is the professional precedent for IQ streams (docs/02 §4 item 5).

## Platform constraints
- Negligible compute, but memory copies and USB-2-class 40 MB/s ingest bound IQ fan-out. Multiple full-rate subscribers are unrealistic; this is an estimate.
- One self-contained device, offline-first. Local sockets are the default; network listeners are optional and must not be required. Don't design out later sharing.
- Consumers may be slow Python scripts. Drop policies matter more than throughput.
- Low-power modes: idle streams must cost nothing (no polling).

## Prior art and reuse
- **SDRangel:** `udpsink`/`remotetcpsink`/`sigmffilesink` sinks plus REST (docs/03 §2.2). Licence: check.
- **SigDigger/Suscan:** UDP sample/symbol broadcast; ZeroMQ plugin (docs/03 §3.4). Licence: check.
- **rtl_433:** JSON/MQTT/InfluxDB outputs, the `messages` pattern (docs/04 §7.5). Licence: check.
- **csdr:** pipe DSP chaining (docs/03 §1.4). **OpenWebRX+:** PSKReporter/APRS-IS/WSPRnet uploads (docs/03 §2.4).
- **Trunk Recorder:** OpenMHz/Broadcastify uploads, a legal-review case (docs/03 §3.5).
- **DeepSig OmniSIG:** VITA-49 in, JSON/SigMF out (docs/03 §3.9).
- **GPL isolation:** a socket or pipe boundary keeps GPL producers and consumers in separate processes. The licence decision is deferred.

## Pitfalls
- A slow or dead consumer blocking the producer and stalling capture: the classic backpressure failure.
- Silent drops. Consumers must see gap frames, or bit-level decoders mis-frame across gaps.
- Wall-clock timestamps from different threads that don't align with sample time.
- Header and version drift breaking consumers. Version the frame format.
- UDP loss and reordering for bits/symbols; UDP fragmentation for large IQ frames.
- Endianness and datatype mismatches (`ci8` vs `cu8`: HackRF is signed 8-bit, `fm_rx.py` reads `int8`).
- Leaking restricted content through a "raw bits" stream that bypasses C22 gating.
- Unauthenticated network listeners on a portable device on public Wi-Fi.

## Testing
- **Loopback contract tests (offline):** replay SigMF fixtures through the pipeline, subscribe with a reference consumer, and assert:
  - header fields match the fixture;
  - sequence continuity;
  - sample-time monotonicity;
  - emission ids;
  - byte-exact bits vs C20 golden output.
- **Interop:**
  - pipe `audio` from an FM broadcast fixture into ffplay/ffmpeg (as `fm_rx.py`);
  - pipe NOAA Weather Radio audio into multimon-ng for SAME;
  - rtl_433 JSON `messages` into an MQTT broker;
  - save an `iq` stream and open it in inspectrum/IQEngine as valid SigMF.
- **Backpressure:** a stalled consumer yields drop-counter growth and gap frames; capture and other subscribers are unaffected; memory stays bounded.
- **Gating:** a POCSAG fixture stream carries metadata without message text; an encrypted-flagged frame has an empty payload.
- **Live hardware:** only for end-to-end latency and throughput benchmarks on the Jetson.

## Example use cases
Provisional until docs/06 §3 mapping:
- SIGNAL-051 — Wireless M-Bus (EU meters)
- SIGNAL-062 — RDS/RBDS & TMC
- SIGNAL-067 — NOAA Weather Radio SAME/EAS
- PROP-002 — Run a multi-band WSPR/FT8 skimmer
- PROP-004 — PSKReporter live band-opening map
- AWARE-038 — Standards-based sensor node
- SIGNAL-008 — All-datalink aggregation
- RESEARCH-008 — Identify line coding
- SPACE-015 — "Space weather now" local dashboard

## Open questions
- **Transport ADR:** pipes plus Unix/TCP sockets, ZeroMQ, gRPC, MQTT, or VITA-49 for IQ? The docs only survey precedents.
- **Scope:** docs/06 C24 bundles the data-plane streams and "the API surface other tools script against". The control API arguably belongs with C39 remote clients or a separate capability.
- Do C22 plugins consume C24 streams? §2.1 draws a linear C19→…→C24 chain that hides this, and C24 actually consumes all of Layer D plus C03/C11.
- **Where 47 USC 605 / restricted-content gating lives** (C22, C24, cross-cutting); network auth.
- Frame schema vs docs/07; public uploads (PSKReporter, SondeHub): C24 or C29?

## Reading list
1. `docs/03 §1.6 "Metadata: SigMF"`
2. `docs/04 §1.3 "Legal considerations (US; not legal advice)"`
3. `docs/03 §2.2 "SDRangel — the most engineering-grade open-source receiver"` (sinks, REST)
4. `docs/01 §7.1 "Worth keeping from PortaPack/Mayhem"` (API-first control, item 4)
5. `docs/02 §3.1 "Throughput math"`
6. `docs/03 §3.4 "Protocol reverse engineering and signal inspection"` (SigDigger UDP/ZeroMQ)
