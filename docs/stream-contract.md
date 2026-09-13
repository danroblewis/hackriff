# Stream-output contract (v1.0)

**Status:** Engineering (T-016, T-014). Implements [ADR-0004](adr/0004-stream-output-contract.md) (PROVISIONAL) and the plugin IPC of [ADR-0003](adr/0003-process-plugin-model.md). Code: `crates/hk-stream/` (contract; re-exported as `hk_api::stream`), `crates/hk-plugins/` (plugin host), `hk stream-tail` (sample consumer). Revised after the T-016/T-014 review (input-class ceiling, metadata allowlist, own-key local-only, gated spectrum cap, process groups, consumer cap).
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
| Unix domain socket | Local consumers (default) | `Listener::bind_uds`. Created **mode 0600** with no window: bound in a fresh 0700 directory, chmodded, renamed into place. A path served by a live listener is refused (probe-connect); a stale socket file is replaced. The only transport for `own-key-decrypted` streams. |
| TCP | Remote consumers | `Listener::bind_tcp`. **Refused for `own-key-decrypted` streams.** **Unauthenticated**: bind to loopback unless the network is trusted. |
| Child stdin | Plugin data plane (§9) | `DecoderFeed`. Never a listener. |
| WebSocket | Browsers | Mapping in §10. The bridge is not implemented yet. |

Each accepted connection is one consumer of one stream: it gets the header, then records from the moment it joined. Consumers never write back; control belongs to the control API.
- **Consumer cap:** at most `PublisherConfig::max_consumers` (default 16) are open; further connections are closed.
- **Hang-ups are reaped while idle:** the accept thread polls every connection it accepted. Because consumers never send, readability (EOF or unexpected bytes) closes the consumer, even when nothing is being published.

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

Content is serialised only if the effective class permits content **and**, when the effective class is `own-key-decrypted`, the header class is also `own-key-decrypted` (`gate::message_content_permitted`). Otherwise `content` is not serialised and `gated: true` is set.

**Own-key content is local-only.** It leaves only on an `own-key-decrypted` stream, and such streams are refused on TCP (and on any future remote bridge); they are served on a mode-0600 Unix socket (`gate::remote_transport_permitted`).

**Binary records.** Payloads of `bits`, `symbols`, `iq` and `audio` are content. On a stream whose header class forbids content:
- the payload is withheld;
- a header-only `GATED` record still goes out, so timing, seq and length metadata flow;
- `publish_binary` returns `StreamError::ContentGated`, so the misrouted producer notices.

**Spectrum.** A waterfall whose row rate reaches the symbol rate is effectively a non-coherent demodulator (POCSAG, voice spectrograms). Under a class that permits content, spectrum streams are ungated. Under a class that forbids content, `Publisher::new` refuses a spectrum stream unless its `sample_rate_hz` (the row rate) is declared and at most **50 rows/s** (`gate::GATED_SPECTRUM_MAX_ROW_RATE_HZ`). The ~30 fps survey waterfall therefore still works under every class, including fail-closed.

**Fail closed.** A missing or unknown class is treated as `metadata-only` wherever it is parsed: headers, plugin output, and records read back by the reference reader.

**Matrix.** This is enforced and unit-tested for every class × kind by `gating_matrix_every_class_by_every_kind`. The test also scans the raw wire bytes for a content sentinel.

| Header class \ kind | messages | bits | symbols | iq | audio | spectrum |
|---|---|---|---|---|---|---|
| `unrestricted` | content (clamped per record; own-key records gated) | payload | payload | payload | payload | payload |
| `own-key-decrypted` (UDS only) | content (clamped per record) | payload | payload | payload | payload | payload |
| `metadata-only` | metadata only | GATED | GATED | GATED | GATED | payload if ≤ 50 rows/s, else refused |
| `restricted-cellular` | metadata only | GATED | GATED | GATED | GATED | payload if ≤ 50 rows/s, else refused |
| `restricted-paging` | metadata only | GATED | GATED | GATED | GATED | payload if ≤ 50 rows/s, else refused |

Gating also covers persistence: the repository refuses content under a forbidding class (`RepoError::GatedContent`, T-002). The plugin host stores the metadata-only form instead (§9.4).

## 7. Backpressure (drop, never block)

- **Per-consumer queue.** Each consumer has a bounded byte ring (`PublisherConfig::queue_bytes`, default 8 MiB), allocated once when it subscribes.
  - A record that doesn't fit is dropped for that consumer only.
  - The drop is counted, and a marker (§5.3) follows.
  - The producer never waits for socket I/O.
- **End of stream.** When the publisher finishes, consumers drain their queues; drops just before the end still get their marker. A consumer still draining after `drain_timeout` (default 5 s) is closed (`CloseReason::DrainTimeout`).
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

## 9. Plugin IPC (T-014, ADR-0003)

Each decoder plugin instance is one subprocess supervised by `hk_plugins::PluginInstance`. The process boundary is also the licence boundary: GPL decoders are only ever run this way (ADR-0010).

### 9.1 Manifest (JSON, `plugins/<id>/manifest.json`)

JSON was chosen because it needs no new dependency and matches the rest of the contract. Unknown fields are errors, so a misspelt `license` is reported, not ignored.

```json
{
  "manifest_version": 1,
  "id": "readsb",                       // [a-z0-9._-]+
  "version": "0.1.0",                   // wrapper version
  "licence": "GPL-3.0-or-later",        // REQUIRED, SPDX where possible
  "description": "…",
  "executable": "readsb",               // bare name: PATH; with '/': relative to the manifest dir
  "args": ["--iformat", "{param.iformat}", "--freq", "{input.center_hz}"],
  "params": {"iformat": "SC16"},
  "input": {
    "kind": "iq",                        // iq | channel | audio | bits
    "datatype": "ci16_le",               // SigMF; iq/channel complex, audio real, bits ru8
    "framing": "raw",                    // hackriff-v1 (default) | raw
    "sample_rates_hz": [2400000],        // empty = any
    "center_hz": {"min": 1089e6, "max": 1091e6},
    "bandwidth_hz": {"max": 2.4e6}
  },
  "output": {
    "format": "ndjson",
    "schema_id": "hackriff.adsb/1",
    "content_class": "unrestricted"      // REQUIRED ceiling, enforced by the host
  },
  "restart": {"backoff_initial_ms": 200, "backoff_max_ms": 30000, "max_restarts": 5, "window_s": 300},
  "limits": {"input_queue_bytes": 8388608, "stall_timeout_ms": 10000,
             "max_message_bytes": 1048576, "stderr_lines": 200, "nice": 10}
}
```

(Comments are for this document only; JSON has none.)

**Argument placeholders:** `{input.sample_rate_hz}`, `{input.center_hz}`, `{input.bandwidth_hz}`, `{input.datatype}`, `{plugin.dir}`, `{param.<name>}`. `{{` and `}}` are literal braces. Unknown placeholders are validation errors.

**`check_input`** refuses an input stream whose datatype, rate, centre or bandwidth the manifest doesn't accept.

### 9.2 Data plane: stdin

- **`hackriff-v1`:** the §4 header (kind `iq` for iq/channel, `audio`, or `bits`), then §5.2 binary records and §5.3 drop markers.
  - The input header's `content_class` is the channel's class, for information only. **The data plane is not gated**: decoders must see samples to decode them. Their output is gated (§9.3–9.4).
  - Plugin input never goes to a listener. `DecoderFeed` attaches only to a child's stdin.
- **`raw`:** payload bytes only, with no header and no markers. This is for existing tools that read raw samples on stdin. Whole records are dropped, so element alignment is kept.
- **Queueing:** the input queue is bounded and never blocks. `PluginInstance::push` returns `Enqueued`, `DroppedFull` or `DroppedDetached` (no process: starting, backoff or failed). The counters obey `offered = enqueued + dropped_full + dropped_detached` exactly.
- **Hang watchdog:** if the queue stays full for `stall_timeout`, the plugin is killed (a hang), counted and restarted.

### 9.3 Message plane: stdout

One JSON object per line; lines longer than `max_message_bytes` are discarded and counted as malformed. stderr lines go to a bounded log ring (`PluginMonitor::log_tail`).

| `type` | Fields | Stored as |
|---|---|---|
| `decode` | `sample_index`, `frame_model` (default `output.schema_id`), `crc_status` (`valid`/`invalid`/`no-crc`/`unknown`, default `unknown`), `identity` `{scheme, value}`, `metadata`, `content`, `content_class` | `Decode`. `decoder_id`/`version` come from the manifest; `demodulation_ref`/`recording_ref` from the plugin context. |
| `annotation` | `value` (label, required), `kind` (`label`/`correction`/`ground-truth`), `confidence` (0–1, default 1), `metadata`, `content`, `content_class` | `Annotation`, author `decoder`. The target is the context's detection, else region, emitter or recording. |
| `log` | `msg` | Log ring |

**Class rule:**
- The **ceiling** is `clamp(manifest output.content_class, input channel content_class)`: a restricted channel fed to an `unrestricted` decoder still yields restricted output.
- A line without `content_class` gets the ceiling.
- A line with one is clamped to the ceiling (§6 ranking), so a plugin can restrict itself further but never upgrade.
- An unknown class string fails closed to `metadata-only`.
- Clamped and unknown lines are counted.

**Metadata allowlist (when a line's effective class forbids content):**
- A manifest whose class forbids content **must** declare `output.metadata_keys` (an empty object is a valid declaration). Each key has a type that cannot carry free text: `integer`, `number`, `boolean`, `hex` or `digits` (with `max_len` ≤ 64), or `enum` (with `values`).
- The host keeps only allowlisted keys with the right type; nested objects, unlisted keys and over-long values are dropped (not truncated: a truncated identifier is a wrong one).
- `frame_model` must be in `output.frame_models`, and an annotation `value` in `output.labels`; otherwise it becomes `output.schema_id`.
- `identity` must match `output.identity` (`scheme`, `charset` hex/digits, `max_len`); otherwise it is dropped.
- With no allowlist (e.g. an `unrestricted` manifest on a restricted channel), nothing survives: metadata `{}`, frame model `schema_id`, no identity.
- Removed fields are counted (`metadata_sanitized`).

**Logs:** plugin `log` lines and stderr are stored in the log ring only when the ceiling permits content; otherwise they are counted (`log_lines_withheld`, `stderr_lines_withheld`). Host errors about malformed lines name the field, never the offending value. `PluginMonitor::log_tail()` returns the lines tagged with the ceiling, so a control API can gate them.

**Time:** the host stamps rows from the line's `sample_index`, using the input anchor and rate (`PluginInstance::set_anchor` after a retune). Plugin wall-clock times are ignored.

### 9.4 Persistence and republish

`hk_plugins::Ingest` (shared as `Arc<Mutex<Ingest>>`) writes rows through `hk_model::Repository`:
- if the repository refuses content under the row's class (`RepoError::GatedContent`), the row is stored **metadata-only** and counted;
- gated content is never persisted;
- each stored row can be republished on a §5.1 messages stream, where §6 gates it again.

### 9.5 Supervision

- **Restart:** an exit is followed by a restart with exponential backoff from `backoff_initial_ms`, doubling up to `backoff_max_ms`. A run longer than `backoff_max_ms` resets the backoff.
- **Crash loop:** more than `max_restarts` exits within `window_s` stops restarts (`PluginState::Failed`), and pushed records then count as `dropped_detached`.
- **Crash isolation:** a crash never touches the producer. `push` has no blocking path.
- **Process groups:** each plugin runs in its own process group. When the leader exits, the host SIGKILLs the whole group *before* reaping the leader, so descendants holding the pipes die and the pgid cannot be reused. Stall kills and shutdown also kill the group.
- **Unblockable input:** the stdin pipe is non-blocking, and the writer polls it together with a wake socket; detaching a plugin (exit, stall, shutdown) abandons a blocked write.
- **Bounded reader join:** output readers are joined for at most 2 s after the group kill. A descendant that escaped the group (e.g. `setsid`) cannot block restart or shutdown; its readers are abandoned (no further ingest) and counted (`readers_abandoned`).
- **Shutdown** SIGKILLs the process group. A pgid is only killed while its leader is unreaped, which rules out reuse.
- **Limits:** only `nice` (via `setpriority`), the queue size, message size and log ring are enforced. Memory and CPU caps are future work.

### 9.6 Fit for readsb (T-015)

readsb is GPL, so it will run as a subprocess against this manifest. Its model:
- `input.kind: "iq"`, `datatype: "ci16_le"` (readsb `SC16`) or `"cu8"` (`UC8`), `framing: "raw"`, `sample_rates_hz: [2400000]`, `center_hz` around 1090 MHz;
- readsb reads the raw samples from stdin (ifile device with `/dev/stdin`);
- its JSON/raw output must become §9.3 `decode` lines, `identity {scheme: "adsb-icao"}`, `crc_status: "valid"`. readsb doesn't print per-message NDJSON on stdout by default, so T-015 will likely need a thin wrapper or a network-output adapter;
- `content_class: "unrestricted"`.

### 9.7 Test plugin

`hk-dummy-plugin` (bin target of `hk-plugins`, manifest `plugins/dummy/manifest.json`) reads framed or raw input and emits one decode per `--every` records. Its test switches:
- `--profile adsb-like`
- `--crash-after K`
- `--stall`
- `--claim-class`, `--content`
- `--annotate`

## 10. WebSocket mapping (browsers)

Browsers can't open raw TCP or UDS sockets (spike S3). A bridge maps a stream one-to-one:
- the header is the **first text message** (the JSON object);
- each record is **one message**: text for `messages` records, binary for binary records;
- the `u32` length prefix is dropped, because WebSocket already frames messages;
- queues and drop policy are the same as §7. The S3 spike used the same drop-on-full bounded queue.

The bridge isn't built yet. It is a follow-up with the UI work (T-023), and may bring an async runtime into its own binary.

## 11. Open issues

- **Authentication** for TCP (and later WebSocket) listeners. They are unauthenticated today, which matters on a portable device on public Wi-Fi.
- **Own-key local-only rule** (§6) is the provisional default endorsed at review; confirm with the user's legal-guardrail pass (docs/06 §5).
- **Gated spectrum cap** of 50 rows/s is a provisional number; revisit with real POCSAG/voice spectrogram fixtures.
- **Manifest trust boundary:** manifests and their executables are trusted code (`plugins/README.md`).
- **Follow-ups recorded by the coordinator:** T-015 datatype conversion stage and raw-framing drop/`sample_index` mapping; T-022 WebSocket bridge (auth, remote rule, consumer cap); control-API exposure of `log_tail`; decode batching and per-plugin rate caps.
- **Replay for missed records** (ADR-0004: "consumers can request replay from a Recording") needs the control API.
- **Crate dependency direction:** resolved. The contract lives in `hk-stream`; `hk-plugins` depends on it (not on `hk-api`), so `hk-api` can later depend on `hk-plugins` for plugin health without a cycle.

## Sources

- [ADR-0004](adr/0004-stream-output-contract.md), [ADR-0003](adr/0003-process-plugin-model.md), [ADR-0010](adr/0010-language-and-licence-ledger.md)
- [C24 stream-output](capabilities/C24-stream-output.md), [C22 decoder-plugins](capabilities/C22-decoder-plugins.md)
- [docs/07 §2.15–2.16](07-data-model.md)
- [spike S3 report](../spikes/s3-web-waterfall/REPORT.md), "Notes for the architecture"
