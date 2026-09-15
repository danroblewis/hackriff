# ADR-0004 — Bitstream and stream-output contract

**Status:** PROVISIONAL
**Touches:** C24 (and the plugin IPC of [ADR-0003](0003-process-plugin-model.md)); Bitstream/Decode objects ([docs/07](../07-data-model.md))

## Context

Workflow step 7: stream bits (and messages, audio, IQ slices) to external programs that turn them into something useful — decoders are pluggable consumers, not hard-coded apps. The same contract carries the internal plugin IPC ([ADR-0003](0003-process-plugin-model.md)) and the external egress. It must frame messages, carry metadata (source emitter, timestamp, frequency, `content_class`), apply backpressure without stalling capture, and enforce restricted-content gating.

## Options

- **Transport:** Unix domain socket / TCP with length-prefixed frames (ubiquitous, no dependency); ZeroMQ (nice pub/sub and backpressure, extra dependency, used by SigDigger/others); named pipes (simplest, one consumer); files (offline replay). 
- **Framing/metadata:** a small binary length-prefixed header + payload, or newline-delimited JSON, or a schema like Cap'n Proto/protobuf.

## Decision (provisional)

- **Default transport: length-prefixed framed messages over a Unix domain socket** (local plugins and local consumers) and **TCP** (remote consumers), with an **optional ZeroMQ binding** for users who want pub/sub. Do not mandate ZeroMQ.
- **Framing:** each stream opens with a **SigMF-style JSON header** (sample type/rate/centre, source emitter id, schema id, `content_class`), then framed records. Bit/symbol streams carry the header once; message streams use newline-delimited JSON records for ease of consumption. IQ slices use SigMF on disk ([docs/07 §2.12](../07-data-model.md)).
- **Backpressure:** bounded queues per consumer with an explicit **drop policy** — a slow consumer is dropped, never the survey. Capture and detection are never blocked by egress. Consumers can request replay from a Recording for what they missed.
- **Metadata on every record:** timestamp, frequency/emitter id, provenance ref, `content_class`.
- **Gating:** stream-output is the enforcement point for restricted content (docs/06 §5). Records with a gated `content_class` are dropped or reduced to metadata before egress and never written to a Recording. Metadata always flows. **Amended (T-143, user decision):** gating is off by default and enabled only by `HK_CONTENT_GATING=1`; by default no class withholds or refuses content, and classes are informational. The opt-in path is not tested.
- **The API is the same surface the UI uses** ([ADR-0002](0002-ui-web-vs-native.md)): a control/query API (state, inventory, history) plus these data streams. Whether the control API and the data streams are one capability or two is the C24 split left open in docs/06 §5 — deferred to implementation; both live in this contract.

## Consequences

- External programs subscribe to a socket and get framed, self-describing streams — the "pluggable consumers" goal, with no coupling to hackriff internals.
- One contract serves both internal plugins and external egress, reducing surface area.
- The drop-not-block rule keeps the real-time path safe under a slow or hostile consumer.
- Gating in one place makes it auditable.
