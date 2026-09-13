# Stream-output contract (v1.0)

**Status:** Engineering (T-016, T-014). Implements [ADR-0004](adr/0004-stream-output-contract.md) (PROVISIONAL) and the plugin IPC of [ADR-0003](adr/0003-process-plugin-model.md). Code: `crates/hk-api/src/stream/` (contract), `crates/hk-plugins/` (plugin host), `hk stream-tail` (sample consumer).
**Legal guardrail:** §6 is the single egress enforcement point for restricted content. Changing it is a core-interface change and needs review.

One contract serves two uses:
- **External consumers** of decoded messages, bits, symbols, IQ, audio and spectra (C24, workflow step 7).
- **Decoder plugins**: their stdin data plane (§9).

## 1. Versioning

- Every stream opens with a header carrying `"schema": "hackriff.stream"` and `"version": "<major>.<minor>"`. This document is **1.0**.
- **Minor versions** may only add:
  - optional header fields;
  - optional message-record fields;
  - new record types and flag bits.

  Readers must ignore unknown fields and skip record types they don't know.
- **A major version** changes framing or existing semantics. Readers refuse a major version they don't speak.

## 2. Transports

| Transport | Use | Notes |
|---|---|---|
| Unix domain socket | Local consumers (default) | `Listener::bind_uds`. A stale socket file is replaced. |
| TCP | Remote consumers | `Listener::bind_tcp`. **Unauthenticated**: bind to loopback unless the network is trusted. |
| Child stdin | Plugin data plane (§9) | `DecoderFeed`. Never a listener. |
| WebSocket | Browsers | Mapping in §10. The bridge is not implemented yet. |

Each accepted connection is one consumer of one stream: it gets the header, then records from the moment it joined. Consumers never write back; control belongs to the control API.

**Threading.** Each consumer has one writer thread and each listener has one accept thread. This uses std threads, not an async runtime:
- consumer counts on a handheld are in single digits;
- a blocked thread costs nothing while its stream is idle, and nothing polls;
- the producer is a synchronous real-time thread anyway;
- no dependency is added.

## 3. Framing

```
frame := u32 length (little-endian) || payload[length]
```

- `length` is at most the header's `max_frame_len`, which is at most **4 MiB** (the protocol maximum).
- The header frame itself is at most **64 KiB**. Readers apply that limit until they have parsed the header.
- Empty payloads are legal.
- Decoders must accept bytes split at any point.
- A length prefix above the limit is a fatal stream error. It is detected from the 4 prefix bytes alone, before any buffer is sized for it. A byte stream cannot resynchronise, so the reader disconnects.

## 4. Header (first frame, JSON object)

| Field | Type | Req | Meaning |
|---|---|---|---|
| `schema` | string | yes | `"hackriff.stream"` |
| `version` | string | yes | `"1.0"` |
| `stream_id` | string | yes | Producer-chosen name, e.g. `decodes/adsb` |
| `kind` | string | yes | `messages`, `bits`, `symbols`, `iq`, `audio` or `spectrum` |
| `content_class` | string | yes | Ceiling class for every record (§6). A reader treats a missing or unknown value as `metadata-only`. |
| `source` | string | yes | Producer, e.g. `hk-plugins:readsb@0.1.0` |
| `emitter_id` | uuid | no | Emitter the whole stream belongs to |
| `provenance_ref` | uuid | no | Provenance of the underlying samples (docs/07 §2.6) |
| `bitstream_id` | uuid | no | Live Bitstream descriptor row (docs/07 §2.16) |
| `datatype` | string | iq/audio | SigMF datatype of payload elements (`ci8`, `cf32_le`, `ri16_le`, `ru8`, ...) |
| `sample_rate_hz` | number | iq/audio | Sample, symbol or spectrum-row rate |
| `center_hz`, `bandwidth_hz` | number | no | RF centre and bandwidth |
| `fft_size` | integer | no | For spectrum streams |
| `framing` | object | no | docs/07 `Framing` (`payload`, `bits_per_symbol`, `symbol_rate_hz`, `schema_id`, `sync_word_hex`), for bits and symbols streams |
| `message_schema` | string | no | Schema id of message `metadata`/`content`, e.g. `hackriff.decode/1` |
| `max_frame_len` | integer | yes | Largest record payload on this stream |
| `record_header_len` | integer | yes | 32 for binary kinds, 0 for `messages` |
| `t_start` | integer | yes | Stream open time, ns since the Unix epoch (UTC) |
| `hackriff_version` | string | yes | Producing build |

## 5. Records

### 5.1 Messages streams: NDJSON records

Each record payload is one JSON object followed by `\n`. Strip the length prefixes and you get a valid NDJSON file.

```json
{"type":"message","seq":12,"t":1757774400123456789,"emitter_id":"0199…","provenance_ref":"0199…",
 "content_class":"unrestricted","gated":false,"decode_id":"0199…","decoder":"readsb@0.1.0",
 "frame_model":"adsb-df17","crc_status":"valid","identity":{"scheme":"adsb-icao","value":"a1b2c3"},
 "metadata":{"icao":"a1b2c3","df":17},"content":{"callsign":"BAW123"}}
```

**Always present:**
- `type`, `seq`, `t`, `content_class` (the effective class after clamping, §6);
- `gated`;
- `metadata`, which always flows.

**Optional fields:**
- `emitter_id`, `provenance_ref`;
- `decode_id` or `annotation_id` (the stored row);
- `decoder`, `frame_model`, `crc_status`, `identity`.

**Content:** `content` is present only when the effective class permits content. `gated: true` means the effective class forbids content: this record carries none, and no record under that class ever will.

`seq` counts per stream from 0. It increases by one for every record the producer publishes, whether or not any consumer queued it.

### 5.2 Binary streams (bits, symbols, iq, audio, spectrum)

Each record is a 32-byte little-endian header followed by the payload:

| Offset | Type | Field |
|---|---|---|
| 0 | u8 | record type: 1 = data, 2 = dropped marker |
| 1 | u8 | flags: bit 0 `GATED`, bit 1 `DISCONTINUITY`, bit 2 `OVERLOAD`, bit 3 `BURST_START`, bit 4 `BURST_END` |
| 2 | u16 | reserved, 0 |
| 4 | u32 | payload length. For `GATED` records this is the withheld length: lengths are metadata, and no payload bytes follow. |
| 8 | u64 | `seq` |
| 16 | i64 | timestamp of the first element, ns since the Unix epoch (UTC) |
| 24 | u64 | stream sample index of the first element (sample time, C03) |

The payload holds whole elements of `datatype`. Producers cannot set `GATED`; only the gate sets it.

### 5.3 Dropped marker

When a consumer's queue was full, the next record that fits is preceded by a marker naming exactly the seqs that consumer missed: `first_seq` through `first_seq + count − 1`.
- **Messages:** `{"type":"dropped","first_seq":N,"count":M,"t":<ns of first dropped>}`
- **Binary:** record type 2 with `seq = first_seq`, flag `DISCONTINUITY`, and the timestamp and sample index of the first dropped record. The payload is `count` as a u64 LE.

A consumer that is disconnected (§7) sees no final marker: the connection just closes.

## 6. Content gating (egress enforcement point)

The enforcement code is `hk_api::stream::gate`, called by `Publisher` before any record byte is produced.
- Metadata always flows. Content is refused unless its class `permits_content()` (`hk_model::ContentClass`).
- Classes are ranked by restrictiveness: `unrestricted` (0) < `own-key-decrypted` (1) < `metadata-only` = `restricted-cellular` = `restricted-paging` (2).

**Messages.** The record's class is clamped to the header class:
- a claim less restrictive than the header is raised to the header class;
- an equal or more restrictive claim is kept.

If the effective class forbids content, `content` is not serialised and `gated: true` is set.

**Binary records.** Payloads of `bits`, `symbols`, `iq` and `audio` are content. On a stream whose header class forbids content:
- the payload is withheld;
- a header-only `GATED` record still goes out, so timing, seq and length metadata flow;
- `publish_binary` returns `StreamError::ContentGated`, so the misrouted producer notices.

`spectrum` payloads are metadata: energy against frequency. The survey waterfall must work under every class, including fail-closed.

**Fail closed.** A missing or unknown class is treated as `metadata-only` wherever it is parsed: headers, plugin output, and records read back by the reference reader.

**Matrix.** This is enforced and unit-tested for every class × kind by `gating_matrix_every_class_by_every_kind`. The test also scans the raw wire bytes for a content sentinel.

| Header class \ kind | messages | bits | symbols | iq | audio | spectrum |
|---|---|---|---|---|---|---|
| `unrestricted` | content (clamped per record) | payload | payload | payload | payload | payload |
| `own-key-decrypted` | content (clamped per record) | payload | payload | payload | payload | payload |
| `metadata-only` | metadata only | GATED | GATED | GATED | GATED | payload |
| `restricted-cellular` | metadata only | GATED | GATED | GATED | GATED | payload |
| `restricted-paging` | metadata only | GATED | GATED | GATED | GATED | payload |

Gating also covers persistence: the repository refuses content under a forbidding class (`RepoError::GatedContent`, T-002). The plugin host stores the metadata-only form instead (§9.4).

## 7. Backpressure (drop, never block)

- **Per-consumer queue.** Each consumer has a bounded byte ring (`PublisherConfig::queue_bytes`, default 8 MiB), allocated once when it subscribes.
  - A record that doesn't fit is dropped for that consumer only.
  - The drop is counted, and a marker (§5.3) follows.
  - The producer never waits for socket I/O.
- **Slow consumers.** A consumer is disconnected, and its transport shut down, when either:
  - its ring stays full for `disconnect_after` (default 5 s); or
  - it drops `disconnect_after_drops` consecutive records (default: never by count).
- **Producer cost.** For each record and each consumer: one lock, a free-space check and one or two `memcpy`s. That is O(consumers).
  - Binary kinds allocate nothing in steady state: rings are preallocated, markers are encoded on the stack, and the consumer list is re-snapshotted only when membership changes. `tests/alloc_free.rs` asserts zero allocations on the producer thread, including the drop and marker paths.
  - A consumer's writer thread holds that consumer's lock for at most a 16 KiB `memcpy`. No I/O ever happens under the lock.
- **Measured** (dev Mac, release, `cargo run --release -p hk-api --example stream_backpressure`), with one consumer that never reads and one reading consumer, both over UDS:
  - 256-byte IQ records: ~7.7 M records/s (~2 GB/s);
  - 16 KiB IQ records: ~6.7 M records/s (the producer drops for both consumers once their rings fill);
  - message records: ~2.5 M records/s.

  In the debug-build test `slow_consumer_is_dropped_not_blocking`, 100 k records publish in bounded time while the reading consumer gets all of them in order.
- **Stats.** `PublisherHandle::consumer_stats()` returns, for each consumer: records enqueued and dropped, drop markers, bytes enqueued/written/discarded, bytes queued, and state/close reason.

## 8. Reading a stream

- `hk_api::stream::StreamReader` is the reference reader. It reads the header, then yields `Record::{Message, Binary, Dropped, Unknown}`.
- `hk stream-tail --uds <path> | --tcp <addr> [--count N]` prints the header as pretty JSON, then one line per record:
  - messages as their NDJSON line;
  - binary records as a summary plus the first 16 payload bytes in hex;
  - markers as `# dropped N records from seq S`.

## 9. Plugin IPC

Added by T-014; see §9 below once present.

## 10. WebSocket mapping (browsers)

Browsers can't open raw TCP or UDS sockets (spike S3). A bridge maps a stream one-to-one:
- the header is the **first text message** (the JSON object);
- each record is **one message**: text for `messages` records, binary for binary records;
- the `u32` length prefix is dropped, because WebSocket already frames messages;
- queues and drop policy are the same as §7. The S3 spike used the same drop-on-full bounded queue.

The bridge isn't built yet. It is a follow-up with the UI work (T-023), and may bring an async runtime into its own binary.

## 11. Open issues

- **Authentication** for TCP (and later WebSocket) listeners. They are unauthenticated today, which matters on a portable device on public Wi-Fi.
- **`own-key-decrypted` content** is permitted on every transport. Whether remote TCP egress should be local-only is a legal-guardrail follow-up (docs/06 §5).
- **Spectrum as metadata** is a design decision recorded here for review.
- **Replay for missed records** (ADR-0004: "consumers can request replay from a Recording") needs the control API.
- **Crate dependency direction.** `hk-plugins` depends on `hk-api` for the codec. If the control API in `hk-api` later needs plugin health, move the stream contract into its own crate to avoid a cycle.

## Sources

- [ADR-0004](adr/0004-stream-output-contract.md), [ADR-0003](adr/0003-process-plugin-model.md), [ADR-0010](adr/0010-language-and-licence-ledger.md)
- [C24 stream-output](capabilities/C24-stream-output.md), [C22 decoder-plugins](capabilities/C22-decoder-plugins.md)
- [docs/07 §2.15–2.16](07-data-model.md)
- [spike S3 report](../spikes/s3-web-waterfall/REPORT.md), "Notes for the architecture"
