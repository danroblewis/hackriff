# Stream-output contract (v1.1)

**Status:** Engineering (T-016, T-014, T-022a, T-043, T-060). Implements [ADR-0004](adr/0004-stream-output-contract.md) (PROVISIONAL) and the plugin IPC of [ADR-0003](adr/0003-process-plugin-model.md). Code: `crates/hk-stream/` (contract; re-exported as `hk_api::stream`), `crates/hk-plugins/` (plugin host), `crates/hk-api/` (WebSocket bridge and read-only HTTP endpoints, §10), `hk stream-tail` (sample consumer). Revised after the T-016/T-014 review (input-class ceiling, metadata allowlist, own-key local-only, gated spectrum cap, process groups, consumer cap), again after the independent re-probe (locality enforced per consumer, metadata policy on publishers, per-row spectrum enforcement, tighter allowlist defaults), and again after T-022a shipped the WebSocket bridge (§10 mapping and auth, §11 residuals).

One contract serves two uses:
- **External consumers** of decoded messages, bits, symbols, IQ, audio and spectra (C24, workflow step 7).
- **Decoder plugins**: their stdin data plane (§9).

## 1. Versioning

- Every stream opens with a header carrying `"schema": "hackriff.stream"` and `"version": "<major>.<minor>"`. This document is **1.1**: 1.0 plus the optional header `audio` profile and the binary `status` record type (T-043, §12).
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
| TCP, token-authenticated | External programs (netcat, Python, GNU Radio) | `hk_api::StreamServer` (§13, T-060): one handshake line names an offered stream or an on-demand opener and carries the API token; then the §3 framing, or one refusal frame. `hk serve` binds `127.0.0.1:8788`. |
| Child stdin | Plugin data plane (§9) | `DecoderFeed`. Never a listener. |
| WebSocket | Browsers | Mapping in §10 (`crates/hk-api/src/bridge.rs`, T-022a). Subscribes as `Locality::Remote`. |
| WebSocket, on demand | Streams opened per request (Listen) | `/ws/open/<name>` (§12, `crates/hk-api/src/ondemand.rs`, T-043): a `StreamOpener` gates and starts the producer, then the connection is bridged as above. |

Each accepted connection is one consumer of one stream: it gets the header, then records from the moment it joined. Consumers never write back; control belongs to the control API.

**Locality rule.** Every egress path goes through one subscription point, `PublisherHandle::subscribe` (listeners, bridges, direct subscribers; plugin stdin through `FeedAttacher`). It enforces locality per consumer, not per listener:
- The writer is an `EgressWriter`, and its `Locality` comes from the type: `UnixStream` is `Local`, `TcpStream` is `Remote`.
- Any other writer must be wrapped in `Declared::local` or `Declared::remote`. `Declared::local` asserts the bytes never leave the host, so it is reviewed code; a `TcpStream` declared local is still `Remote`.
- A `Remote` consumer of an `own-key-decrypted` stream is refused before anything is queued (`StreamError::LocalOnly`, counted in `gate_stats().remote_consumers_refused`). `bind_tcp` also refuses such streams up front.
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
| `version` | string | yes | `"1.1"` (readers accept any `1.x`) |
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
| `audio` | object | no (1.1) | Audio profile for `audio` streams: mode chosen automatically, estimated parameters, squelch, AGC (§12). Metadata only; invalid on other kinds. |
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
| 0 | u8 | record type: 1 = data, 2 = dropped marker, 3 = status (1.1, §12) |
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
- **Gated (binary, gated spectrum only):** the same record with flags `GATED | DISCONTINUITY`. It reports rows **withheld by the egress gate** (§6, spectrum enforcement), not queue drops. Its timestamp and sample index are those of the next delivered row (the last delivered row at end of stream), so withheld rows contribute only their count. The reference reader returns `Record::Dropped(DropMarker { gated: true, .. })`.

A consumer that is disconnected (§7) sees no final marker: the connection just closes.

## 6. Content gating (egress enforcement point)

The enforcement code is `hk_api::stream::gate`, called by `Publisher` before any record byte is produced.
- Metadata always flows. Content is refused unless its class `permits_content()` (`hk_model::ContentClass`).
- Classes are ranked by restrictiveness: `unrestricted` (0) < `own-key-decrypted` (1) < `metadata-only` = `restricted-cellular` = `restricted-paging` (2).

**Messages.** The record's class is clamped to the header class:
- a claim less restrictive than the header is raised to the header class;
- an equal or more restrictive claim is kept.

Content is serialised only if the effective class permits content **and**, when the effective class is `own-key-decrypted`, the header class is also `own-key-decrypted` (`gate::message_content_permitted`). Otherwise `content` is not serialised and `gated: true` is set.

**Message metadata policy.** When a message's effective class forbids content, `publish_message` reduces every field outside `content` to the publisher's `MetadataPolicy`, whoever produced the record (plugin republish or in-process producer). `hk_stream::policy` holds the same typed allowlist model as plugin manifests (§9.3):
- metadata keys that are not allowlisted, or have the wrong type, are dropped;
- `frame_model` must be in `frame_models` (for annotations, `labels`) or equal the header's `message_schema`; otherwise it becomes `message_schema`, or is omitted if there is none;
- `identity` must match the policy's identity shape; `decoder` must be a token (`[A-Za-z0-9_.:/@+-]{1,64}`);
- removed fields are counted (`gate_stats().metadata_fields_sanitized`).

A messages publisher whose header class forbids content **cannot be created without a policy**: `Publisher::new` returns `MetadataPolicyRequired`; use `Publisher::with_metadata_policy`. On a permitting stream without a policy, restricted records fall back to the empty allowlist. A plugin republisher carries the manifest's policy, with `message_schema` set to `output.schema_id`.

**Persistence path.** The repository refuses restricted *content* but stores whatever metadata it is given. Restricted `Decode`/`Annotation` rows must therefore pass through `hk_stream::policy::sanitize_decode` / `sanitize_annotation` before `Repository::insert_*`. Plugin ingest does this (§9.3); in-process producers must. As a fail-closed check, `hk_plugins::Ingest` tests each restricted row's shape (`decode_is_allowlist_shaped`: flat typed metadata, token frame model, hex/digits identity). A non-conforming row is reduced to the empty allowlist before storage and counted (`IngestStats::rows_stripped`).

**Own-key content is local-only.** It leaves only on an `own-key-decrypted` stream. Such streams are served on a mode-0600 Unix socket and refused to every remote consumer, per consumer, by the locality rule (§2; `gate::remote_transport_permitted`).

**Binary records.** Payloads of `bits`, `symbols`, `iq` and `audio` are content. On a stream whose header class forbids content:
- the payload is withheld;
- a header-only `GATED` record still goes out, so timing, seq and length metadata flow;
- `publish_binary` returns `StreamError::ContentGated`, so the misrouted producer notices.

**Spectrum.** A waterfall whose row rate reaches the symbol rate is effectively a non-coherent demodulator (POCSAG, voice spectrograms). Under a class that permits content, spectrum streams are ungated.

Under a class that forbids content, `Publisher::new` refuses a spectrum stream unless it declares all three of:
- `sample_rate_hz` (the row rate), at most **50 rows/s** (`gate::GATED_SPECTRUM_MAX_ROW_RATE_HZ`; otherwise `SpectrumRowRate`);
- `fft_size`;
- a known `datatype` (otherwise `SpectrumGeometry`).

The declaration is then **enforced on every row** in `publish_binary`:
- **Rate.** A row needs a token from two buckets, each refilled at the declared rate with depth `GATED_SPECTRUM_BURST_ROWS` (2): one over wall-clock arrival, one over the spacing of the rows' `t`. A `t` that goes backwards is refused. `sample_index` is not used, because for spectrum its unit is the underlying IQ sample, which the header does not describe.
- **Size.** The payload is at most `fft_size` × the element size of `datatype`.

A row over either cap is withheld entirely:
- No header-only record is sent, because per-row `t`/`sample_index` at the offered rate would itself be a channel.
- It consumes a seq, is counted (`gate_stats().spectrum_rows_gated`), and `publish_binary` returns `SpectrumGated`.
- Consumers get one counted `GATED` marker (§5.3) for the withheld run, before the next delivered row or at end of stream.

The ~30 fps survey waterfall therefore still works under every class, including fail-closed. A producer should declare its rate with some margin. A replay faster than real time on a gated stream is throttled; after rewinding `t`, open a new publisher.

**Fail closed.** A missing or unknown class is treated as `metadata-only` wherever it is parsed: headers, plugin output, and records read back by the reference reader.

**Matrix.** This is enforced and unit-tested for every class × kind by `gating_matrix_every_class_by_every_kind`. The test also scans the raw wire bytes for a content sentinel.

| Header class \ kind | messages | bits | symbols | iq | audio | spectrum |
|---|---|---|---|---|---|---|
| `unrestricted` | content (clamped per record; own-key records gated; restricted records policy-reduced) | payload | payload | payload | payload | payload |
| `own-key-decrypted` (local consumers only) | content (clamped per record; restricted records policy-reduced) | payload | payload | payload | payload | payload |
| `metadata-only` | policy-reduced metadata (policy required) | GATED | GATED | GATED | GATED | payload if declared ≤ 50 rows/s with geometry; rate and size enforced per row |
| `restricted-cellular` | policy-reduced metadata (policy required) | GATED | GATED | GATED | GATED | payload if declared ≤ 50 rows/s with geometry; rate and size enforced per row |
| `restricted-paging` | policy-reduced metadata (policy required) | GATED | GATED | GATED | GATED | payload if declared ≤ 50 rows/s with geometry; rate and size enforced per row |

The re-probe regressions (locality per consumer, in-process metadata policy, per-row spectrum enforcement) are `crates/hk-stream/tests/egress_gaps.rs`.

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
- A manifest whose class forbids content **must** declare `output.metadata_keys` (an empty object is a valid declaration). Each key has a type that cannot carry free text: `integer`, `number`, `boolean`, `hex` or `digits`, or `enum` (with `values`).
- The host keeps only allowlisted keys with the right type; nested objects, unlisted keys and over-long values are dropped (not truncated: a truncated identifier is a wrong one).
- `frame_model` must be in `output.frame_models`, and an annotation `value` in `output.labels`; otherwise it becomes `output.schema_id`, which must itself be a token (`[A-Za-z0-9_.:/-]{1,64}`).
- `identity` must match `output.identity` (`scheme`, `charset` hex/digits, `max_len`); otherwise it is dropped.
- With no allowlist (e.g. an `unrestricted` manifest on a restricted channel), nothing survives: metadata `{}`, frame model `schema_id`, no identity.
- Removed fields are counted (`metadata_sanitized`).
- The model and the sanitising functions are shared with egress (`hk_stream::policy`, §6).

**Allowlist defaults (covert-channel budgets).** A policy only ever applies under a restricted class, so every typed field is a budget. Manifests are trusted but reviewed (§11):
- `hex`/`digits` keys and `identity` have a default `max_len` of **8**. A longer value (up to 64) needs an explicit `max_len` **and** a non-empty `review_note` string on that key or identity; without the note the manifest is refused.
- Reviewed long strings and every `integer`/`number` key (up to 64 bits per record) are reported as warnings in `PluginManifest::warnings`. `PluginManifest::load` prints them to stderr, and the host copies them into the plugin's log ring.
- **`sample_index` bound.** A restricted line's `sample_index` becomes the row's time, so it must lie within the inclusive range of input offered to that plugin instance: from the lowest record `sample_index` to the highest `sample_index + elements`. A line with an index outside that range, a non-integer index, or an index before any input is offered is dropped and counted (`sample_index_out_of_range`). Lines without `sample_index` get host arrival time.
- **Confidence.** A restricted annotation's `confidence` is rounded to 0.01.
- **Example paging policy (not a plugin):** `crates/hk-plugins/policies/restricted-paging.json` (`hk_plugins::EXAMPLE_RESTRICTED_PAGING_OUTPUT`). It allowlists only `capcode` (digits, at most 8), `function` (enum `0`–`3`), `baud` (enum `512`/`1200`/`2400`) and `encoding` (enum `numeric`/`alpha`/`tone`). `t` is host-stamped from `sample_index`. Message bodies, numeric pages included, are content and are never allowlisted.

The N1 covert-channel regressions (hex text, packed integer, numeric page in capcode, `sample_index` offset, confidence digits) are `crates/hk-plugins/tests/egress_gaps.rs`.

**Logs:** plugin `log` lines and stderr are stored in the log ring only when the ceiling permits content; otherwise they are counted (`log_lines_withheld`, `stderr_lines_withheld`). Host errors about malformed lines name the field, never the offending value. `PluginMonitor::log_tail()` returns the lines tagged with the ceiling, so a control API can gate them.

**Time:** the host stamps rows from the line's `sample_index`, using the input anchor and rate (`PluginInstance::set_anchor` after a retune). Plugin wall-clock times are ignored.

### 9.4 Persistence and republish

`hk_plugins::Ingest` (shared as `Arc<Mutex<Ingest>>`) writes rows through `hk_model::Repository`:
- if the repository refuses content under the row's class (`RepoError::GatedContent`), the row is stored **metadata-only** and counted;
- gated content is never persisted;
- a restricted row that is not allowlist-shaped (it skipped the policy) is reduced to the empty allowlist first and counted (`rows_stripped`);
- each stored row can be republished on a §5.1 messages stream, where §6 gates it again. The republisher applies its own metadata policy to restricted rows, so a republisher for restricted plugins is created with `Publisher::with_metadata_policy` (the manifest's policy, `message_schema` = `output.schema_id`). Without one, restricted rows go out with metadata `{}`.

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

Browsers can't open raw TCP or UDS sockets (spike S3). Built in T-022a: `crates/hk-api/src/bridge.rs`
(`bridge::attach`, `WsSink`), served at `GET /ws/<stream_id>` by `crates/hk-api/src/http.rs`. It runs
on **std threads and `tungstenite`**, not an async runtime — one accept thread per `hk-api` server
(shared with the plain HTTP endpoints) and one writer thread per consumer, matching §2's threading
rationale.

**Mapping (1:1).** A bridge connection is subscribed through the same `PublisherHandle::subscribe`
as every other consumer (§2), so it sees only records the gate has already passed:
- the header is the **first text message** (the JSON object, verbatim);
- each later record is **one WebSocket message**: text for `messages` streams (the NDJSON line or
  drop marker, verbatim including its trailing `\n`), binary for every binary kind (the §5.2 32-byte
  record header + payload, or a §5.3 marker);
- the `u32` length prefix is dropped, because WebSocket already frames messages;
- queues and drop policy are unchanged from §7: the publisher's per-consumer writer thread blocks
  only on that browser's TCP socket, its bounded ring fills, records are dropped with markers, and
  it is disconnected after `disconnect_after`. There is no second queue in the bridge.

**Locality: always remote.** Every browser connection subscribes wrapped in `Declared::remote`,
**even from `127.0.0.1`** — a page can forward what it receives, so it is treated as remote-capable
regardless of where the socket originates. Consequently an `own-key-decrypted` stream is refused
before anything is queued (`StreamError::LocalOnly`, `gate_stats().remote_consumers_refused`),
answered as **HTTP 403** with no WebSocket upgrade (the `101` response is written by the sink only
after `subscribe` succeeds, so a refusal never upgrades the connection). Every other gate (class
clamping, gated-spectrum rate/size, §6) applies unchanged; the bridge adds no gating of its own.

**Other refusals, also plain HTTP before any upgrade:**
- **HTTP 503** at `PublisherConfig::max_consumers` (`StreamError::TooManyConsumers`);
- **HTTP 410** for a stream that has already finished (`StreamError::Finished`);
- **HTTP 404** for an unknown `stream_id`;
- **HTTP 426** for a request to `/ws/<id>` that isn't a valid WebSocket upgrade (missing
  `Upgrade: websocket`/`Connection: Upgrade`, or `Sec-WebSocket-Version` other than `13`).

**Authentication (token).** `/ws/<id>`, like every `/api/*` path, requires the server's bearer
token (`crates/hk-api/src/auth.rs`): a 256-bit value generated at start from the OS CSPRNG, or taken
from `HK_TOKEN` (≥16 printable-ASCII characters, no spaces). Sent as `Authorization: Bearer` where a
header can be set, or `?token=` for browser `WebSocket` connections, which cannot set headers.
Comparison is constant time over the full length (`Token::verify`). A missing or wrong token is
**HTTP 401**, returned before the WebSocket handshake and before anything about the stream (even
whether it exists) is revealed.

**Bind address and transport security.** `hk serve` binds `127.0.0.1` by default. Binding a
non-loopback address (e.g. `0.0.0.0`) exposes the bridge, and every other `/api/*` endpoint, to
everyone who can reach that interface — the LAN or, on open Wi-Fi, anyone nearby. **There is no TLS
in M0**: the token is the only protection and travels, and is compared, in cleartext. `hk serve`
prints a warning when it binds non-loopback.

**Caps.** In addition to the per-stream `max_consumers` (§2) and per-consumer queue (§7), the HTTP
server bounds request size and concurrency: request heads are capped at 16 KiB and must complete
within `request_timeout` (default 10 s), and at most `ServerConfig::max_connections` (default 64)
connection threads run at once, WebSocket consumers included — a further TCP connection is simply
not accepted until one frees up. `/api/history` and `/api/floor` (below) additionally cap query
result size.

**Consumers never write.** As in §2: any byte a browser sends (a close frame included) or a hang-up
is read by `watch_peer` as the signal to close that consumer and shut its socket down.

**Read-only control/query endpoints.** The bridge shares its `hk-api` HTTP server with four
`GET`, token-authenticated JSON endpoints that are **not part of the framed stream contract** above
— they return plain JSON, not header/record framing — but are worth naming here because they run
under the same auth and gating:
- `/api/streams`: discovery (§13.2) — the offered streams' header metadata only (id, kind, class,
  geometry, `content_permitted`, `remote_permitted`, open consumer count, `format`, `tcp_target`),
  the on-demand openers and the TCP stream server address — never content;
- `/api/history?f_lo&f_hi&t0&t1[&max_cells]`: the T-017 region-over-time grid;
- `/api/floor?f_lo&f_hi&t0&t1[&max_steps]`: the T-021 floor-vs-time series;
- `/api/inventory?[f_lo&f_hi][&t0&t1][&status][&tag][&scheme][&family][&cursor][&limit]`: one
  page (≤ 500 rows, cursor ≤ 10⁶) of the T-018 signal inventory.
  - **Built only on `Repository::query_inventory`**, always with `IdentityAccess::Standard`. No
    request parameter grants the own-traffic authorisation over HTTP in M0.
  - **Identity values:** `identity_value` is present only when the query returned it in clear
    (class `unrestricted`). Otherwise the row carries `withheld: true`, `identity_scheme` and
    `identity_class`, and no value.
  - **Status reasons:** on withheld rows, a status reason from an author that may have seen the
    identity (decoder, user, system) is withheld too.
  - **Never included:** decode content, fingerprints and links. A test scans hk-api's sources
    for the ungated emitter getters.

See `crates/hk-api/src/http.rs` and `crates/hk-api/src/query.rs` for their shapes and caps; `ui/README.md`
documents the wire format and security notes from the UI's point of view.

## 11. Open issues

- **Authentication.** The plain TCP listener (§2) is still unauthenticated; that matters on a portable device on public Wi-Fi. The T-060 stream server (§13) checks the API token in its handshake line, in cleartext like the bridge. The WebSocket bridge (§10) now has bearer-token auth, but **TLS and per-user auth do not exist**: the token travels and is compared in cleartext, one token authorizes every client, and there is no revocation short of restarting the server.
- **Gated spectrum cap** of 50 rows/s (burst 2) is a provisional number; revisit with real POCSAG/voice spectrogram fixtures.
- **Manifest trust boundary: trusted but reviewed** (policy recorded by the coordinator). Manifests and their executables are trusted code (`plugins/README.md`). The host contains *accidental* leaks from well-meaning decoders; it does not contain a malicious executable. A manifest's class, allowlist, `max_len` budgets and `review_note`s are guardrail declarations and are reviewed like code. The defaults (§9.3) make anything beyond a small budget explicit and visible as a load warning.
- **Residual side channels (noted, not fixed).** These remain open to a producer that modulates them deliberately, under every class:
  - **Timing and ordering:** arrival times, inter-record spacing, record order, seq gaps and drop/gated-marker counts.
  - **Time fields:** the `t` of a restricted record carries about log2(input range) bits via an in-range `sample_index`, or host arrival time for lines without one.
  - **Typed values within their bounds:** an 8-digit capcode (~26.6 bits), an enum choice, `integer`/`number` keys (64 bits, warned at load), confidence (~7 bits), `crc_status`, record and payload lengths (including the withheld length of `GATED` records).
  - **Gated spectrum:** delivered rows' `t` and `sample_index` (up to 128 bits per row at ≤ 50 rows/s), withheld-run counts, and bin values. A spectrum is metadata by rule, but a producer can modulate its bins.
  - **Ids from in-process producers:** `emitter_id`, `provenance_ref`, `decode_id` and `annotation_id` are opaque UUIDs that trusted in-process code chooses.
  - **Locality:** it relies on writer types; `Declared::local` around a network-backed writer is a review error that the type check cannot see, except for a bare `TcpStream`.
- **WebSocket bridge residuals (T-022a, noted at merge, not fixed):**
  - **No TLS.** The token and every byte of every stream travel in cleartext (§10, §11 authentication above).
  - **`--loop` replay reconnects browsers on every pass.** `hk serve --replay --loop` cannot rewind a gated publisher's `t` in place, so each pass opens a new publisher under the same `stream_id`; existing WebSocket consumers see their stream finish (closing the socket) and the page reconnects to the new one, rather than the bridge presenting one continuous stream across passes.
  - **A race for the last consumer slot** can reset the TCP connection instead of returning a clean HTTP 503: two upgrade requests arriving as `PublisherConfig::max_consumers` is reached can both pass the check before either subscribes, so the loser's connection drops rather than receiving the 503 body.
  - **No `units` field on the stream header.** The v1.0 header (§4) doesn't carry physical units for binary payloads; `hk serve`'s spectrum rows are `rf32_le` dBFS/Hz by producer convention (`crates/hk-cli/src/serve.rs`) only, documented in `ui/README.md`, not asserted by the contract. Proposed as a **v1.1** optional header field (`units`, minor version per §1); not implemented.
- **Follow-ups recorded by the coordinator:** T-015 datatype conversion stage and raw-framing drop/`sample_index` mapping (a restricted plugin's `sample_index` must be the host's record index to pass the §9.3 bound); control-API exposure of `log_tail`; decode batching and per-plugin rate caps.
- **Replay for missed records** (ADR-0004: "consumers can request replay from a Recording") needs the control API.
- **Crate dependency direction:** resolved. The contract lives in `hk-stream`; `hk-plugins` depends on it (not on `hk-api`), so `hk-api` can later depend on `hk-plugins` for plugin health without a cycle.

## 12. Audio streams and on-demand streams (1.1, T-043)

### 12.1 On-demand streams

Some streams exist only because a consumer asked for them, e.g. listening to one emitter. `hk_stream::ondemand` defines the transport-agnostic shape; T-060 reuses it for bits and symbols:
- **`StreamOpener::open(&OpenRequest) -> Result<OpenedStream, OpenRefusal>`.**
  - The request holds the transport's query parameters, with `token` removed before any opener sees them.
  - An opener **gates before it attaches anything**. A refusal carries an HTTP-style `status`, a `code` token, a `reason` and, for gate refusals, the `content_class`. It never carries content.
- **`OpenedStream { header, handle, session }`.**
  - The front end subscribes its connection to `handle` exactly as for any stream, so §6 gating, sequence numbers, §5.3 markers and §7 drop-not-block apply unchanged.
  - Dropping `session` stops the producer.
  - A producer that stops on its own (idle, source gone) finishes its publisher, which closes the connection.
- **`OpenerRegistry`** maps names to openers.
- **WebSocket front end:** `GET /ws/open/<name>?<params>&token=…` (`crates/hk-api/src/ondemand.rs`).
  - Token first: `401`.
  - Upgrade checks: `426`/`400`. Unknown name: `404`.
  - A **refusal completes the upgrade**, sends one text message `{"type":"refused","status","code","reason","content_class"}`, then closes with code **4000 + status** (e.g. 4403 gate refusal, 4503 at capacity). Browsers cannot read the body of a failed upgrade.
  - Otherwise the connection is bridged as a remote consumer (§10). The browser sending anything, or hanging up, drops the session.
  - **Liveness (T-066):** the server pings every `ondemand_ping_interval` (5 s); a peer that sends nothing, not even the automatic pong, for `ondemand_peer_timeout` (20 s) is treated as gone (half-open connection, vanished tunnel client) and its session is dropped. A write blocked for the peer timeout fails the consumer too.
  - **While the run re-plumbs** into a new window, requests are refused with `503 replumbing` (retry); `410 source-ended` means the run has ended.
  - **Listen admission (T-066):** at most `max_listeners` chains (default 8) and an estimated CPU budget (`cpu_fraction` of the cores, each chain costed from its tuned sample rate and mode); beyond either, `503 busy` with both counts in the reason. `/api/status` `listen.budget` reports `{max_listeners, listeners, running, cores, used_cores}`.
  - **One per-run chain budget (T-071, `hk_pipeline::chains::budget`):** Listen chains and burst taps (§13, including those output recordings open) are admitted together: at most `max_chains` (default 16; a larger per-kind limit raises it), `max_listeners` (8) and `max_taps` (8), against one CPU budget (a tap costs `tap_cores`, 0.01). Refusals are `503 busy` naming the limit hit (`listener limit`, `burst tap limit`, `chain budget`, `CPU budget`) with the counts and cores; running chains are untouched. `/api/status` `budget` reports `{max_chains, chains, max_listeners, listeners, max_taps, taps, cores, used_cores, refused_busy}` and `chain_stats[]` each running chain's own counters `{id, kind, stream_id, center_hz, bandwidth_hz, age_s, samples, lost_samples, cpu_s, cpu_load, latency_ms_last, latency_ms_max, backlog_ms, records, consumers, dropped}` (thread CPU time; `dropped` is the records its stream dropped for slow consumers). Chains share only the ring: several streams of one kind run concurrently, each with its own channel, refinement, publisher and counters. Neighbouring analog chains that refine to the same off-raster emission are deduplicated: the chain whose channel is nearer owns it (`chains.duplicate_emission` counts the other).

### 12.2 Audio profile

- **Header:**
  - `kind: "audio"`, `datatype: "ri16_le"` (mono), `sample_rate_hz: 48000`;
  - `center_hz`/`bandwidth_hz`: the demodulated RF channel;
  - `emitter_id` when an emitter was requested;
  - `audio`: `{channels, frame_samples, mode, mode_confidence, mode_rules, params, snr_db, squelch, agc, deemphasis_s, demod}`.
    - `mode` is chosen by auto-mode selection (`wfm`, `nbfm`, `am`, `usb`, `lsb`, `cw`); there is no manual mode.
    - `params` is docs/07 `EstimatedParams`.
    - `squelch` is `{open_snr_db, hysteresis_db, noise_dbfs}`; `agc` is `{enabled, target_dbfs, max_gain_db}`.
    - `refinement` (optional, T-070): present when the channel was refined from the demodulator's own output (`hk_pipeline::refine`). `{provenance: "refined by output analysis", objective, center_hz, bandwidth_hz, start_center_hz, start_bandwidth_hz, quality, converged, iterations, evaluations, elapsed_s, mode_params, labels}`. The header's `center_hz`/`bandwidth_hz` and `params.bandwidth_hz`/`cfo_hz`/`pilot_hz` are then the refined values; the start values are the selection or detection. Readers that ignore unknown fields are unaffected.
- **Data records** (type 1):
  - payload: `frame_samples` (960, i.e. 20 ms) `i16` LE samples;
  - `sample_index`: audio samples since the stream start;
  - `t`: time of the first sample.
  - A jump in `sample_index` is a gap (squelch closed, or samples skipped to stay live), and the next record is flagged `DISCONTINUITY`. A `seq` gap is loss.
- **Status records** (type 3):
  - 32-byte header; the payload is a flat JSON object of numbers, booleans and short tokens (`policy::metadata_is_allowlist_shaped`, enforced by `Publisher::publish_status`), so no free text rides on it.
  - Audio fields: `level_dbfs`, `snr_db`, `squelch_open`, `agc_gain_db`, `frames`, `squelched_frames`, `lost_samples`, `latency_ms`, `backlog_s`, sent about every 250 ms.
  - Refinement fields (T-070): `refined_center_hz` and `refined_bandwidth_hz` (the refined channel in force, absent when not refined) and `refine_updates` (background re-refinements that retuned the channel after passing the hysteresis).
  - They take a `seq`. The reference `StreamReader` returns them as `Record::Unknown`; `record::parse_status_record` decodes them.
- **Gating:**
  - Audio payloads are content: under a class that forbids content the egress gate withholds them (§6), as for any audio stream.
  - The listen opener refuses earlier, before a ring read; see `hk_pipeline::chains::listen` for the rule. Restricted bands are refused whatever the source class. Unclassified content (a fail-closed `metadata-only` source without a user classification rule) is refused.

## 13. External programs: TCP stream server, discovery, burst bits and symbols (T-060)

Workflow steps 6–7: demodulated outputs leave hackriff for pluggable consumers. Nothing in §3–§7
changes; this section adds a transport, a discovery document and a record profile. The version
stays **1.1**.

### 13.1 TCP stream server (`crates/hk-api/src/tcp.rs`)

- **Handshake.** After connecting, the client sends one line (≤ 4096 bytes, `\n`-terminated, an
  optional `\r` stripped) within 10 s:
  - `<stream_id>?token=<token>`: an always-on stream from the registry (e.g. `spectrum/live`,
    `bits/fsk-bursts/<emitter>`); no other parameters.
  - `open/<name>?token=<token>[&k=v...]`: an on-demand opener (§12.1): `open/bits`,
    `open/symbols`, `open/listen?emitter=<id>`. The token is removed before the opener sees the
    parameters.

  Values are percent-decoded; a leading `/` is ignored. The token is the API token (`HK_TOKEN` or
  the token file), compared in constant time.
- **Response.** The §3 byte stream exactly as on a Unix socket (header frame, then records), or a
  **refusal frame** instead of the header: one frame whose JSON object is
  `{"type":"refused","status","code","reason","content_class"}`, after which the connection closes.
  Statuses: 400 bad handshake (including bytes after the line), 401 token, 403 local-only or legal
  refusal, 404 unknown stream or opener, 408 handshake timeout, 410 finished, 431 line too long,
  503 at capacity. **Nothing about streams is revealed before the token verifies**: a wrong token
  gets the same 401 whether or not the target exists. A reader tells a header from a refusal by
  `schema` vs `type`.
- **Consumers never send.** Any byte after the handshake line, or a hang-up (EOF), closes the
  consumer and drops an on-demand session, which stops its producer. Tools that half-close on
  stdin EOF end the stream at once: `nc` (BSD and OpenBSD) keeps its sending side open by default;
  use `socat -t <large>`.
- **Locality and gating.** The connection subscribes as a `TcpStream` (`Locality::Remote`):
  `own-key-decrypted` streams get 403; §6 gating applies unchanged.
- **Backpressure.** The publisher's per-consumer queue is the only queue (§7). A client that stops
  reading loses records with markers and is disconnected after `disconnect_after`; the producer
  never waits. `StreamServer::stats()` reports accepted, refused, served, open connections and the
  records enqueued and dropped for its consumers; `StreamServer::consumers()` gives each open
  connection's own counters (kept per stream by that stream's publisher).
- **Many streams at once.** Every connection is one consumer of one stream; a client opens several
  connections for several streams (e.g. two emitters' bits, or three Listen channels). Stream ids,
  sessions and drop counters are per stream; the test
  `concurrent_streams_of_one_kind_are_isolated_with_per_stream_drop_counters` covers isolation.
- **Bounds.** At most 32 connection threads (a further connection gets a 503 refusal frame).
- **Binding.** `hk serve` starts it on `HK_STREAM_TCP` if set, else `127.0.0.1:8788`, else an
  ephemeral loopback port; the address is printed and reported by discovery.

One-liner (hex dump of the bits of every demodulated burst):

```sh
printf 'open/bits?token=%s\n' "$HK_TOKEN" | nc 127.0.0.1 8788 | xxd | head -40
```

Python clients (standard library only): `py/examples/` (`hkstream.py` parser, `hk_bits.py`,
`hk_audio_wav.py`), documented in `py/README.md`.

### 13.2 Discovery (`GET /api/streams`)

Token-authenticated JSON, metadata only:
- `streams[]`: the §10 listing plus `tcp_target` (the stream id) and `format` (`record_header_len`,
  `max_frame_len`, `emitter_id`, `bitstream_id`, `bit_framing` (docs/07 `Framing`), `audio`,
  `message_schema`).
- `on_demand[]`: `name`, `ws_path` (`/ws/open/<name>`), `tcp_target` (`open/<name>`), plus what the
  opener describes (`StreamOpener::describe`): `kind`, `datatype`, `params`, `records`.
- `tcp`: `{addr, handshake, refusal}`, or `null` when no TCP stream server runs.

### 13.3 Burst bits and symbols (`open/bits`, `open/symbols`)

Code: `hk_stream::bursts` (profile), `hk_pipeline::chains::taps` (producer).
- **Request.** No parameter: every burst the run demodulates. Otherwise `emitter=<id>`,
  `detection=<id>` or `f_lo=<Hz>&f_hi=<Hz>` (bursts whose centre lies in that extent, padded by
  25 % or 2 kHz, or bursts stored under that emitter). A mode or parameter is refused (400). Each
  request is its own tap and stream (`bits/bursts/<n>`), so concurrent taps on different emitters
  are independent.
- **Source.** Bursts demodulated by the `fsk-bursts` chains, offered while a chain runs (once
  `min_bursts` are in, framing inferred over the bursts so far, at most every 500 ms, only while a
  tap is open) and at chain detach (final framing, emitter id). A tap costs nothing without bursts.
  At most 8 taps; the end of the run finishes every tap stream.
- **Header.** `kind` `bits` with `datatype` `ru8` (one byte per bit, 0 or 1), or `kind` `symbols`
  with `datatype` `rf32_le` (one soft value per symbol, LLR-like, positive = 1). `framing`
  `{payload: hard-bits | soft-symbols, bits_per_symbol: 1}`; `center_hz`, `bandwidth_hz` and
  `emitter_id` describe the target when there is one.
- **Per burst, two records:**
  1. **Status** (type 3), a flat object: `burst` (per-stream counter), `symbols` (elements in the
     data record), `symbol_rate_bd`, `f_center_hz`, `bandwidth_hz`, `snr_db`, `framed`, `inverted`,
     `sync_bit`, `sync_bits`, `payload_bit`, `payload_bits`, `bit_order` (`msb-first`/`lsb-first`),
     `crc` (`valid`/`invalid`), `emitter_id` (at detach), `content_withheld`. Absent values are
     omitted.
  2. **Data** (type 1), flags `BURST_START | BURST_END`, `t` of the first symbol, `sample_index`
     the source sample of the first symbol centre. Bits and symbols are in the **framing model's
     polarity** (inverted bursts complemented), so `payload_bit`/`payload_bits` index straight
     into the payload; pack with `bit_order` to get the payload bytes.
- **Classes (existing FSK rules, not extended).** A burst's class is the one its Decode rows are
  stored under (fail closed unless a user classification rule vouches for the emitter). The header
  class is decided at open: `classify_emitter` on the requested extent (fail closed when nothing
  vouches); for every burst, the source class, or `unrestricted` when classification rules exist
  and the source is not restricted. A burst whose class withholds content on a permitting stream
  sends only its status record, with `content_withheld: true`. Under a header class that forbids
  content, data records go out header-only (`GATED`, §6).
- **Symbols gap.** Soft symbols exist only where a chain recovers them: the FSK burst chain today.
  Analog Listen streams audio, not symbols; plugin decoders (readsb) publish messages; there is no
  PSK/QAM symbol recovery in the pipeline yet, so no symbols stream exists for those signals.
- **Always-on per-emitter streams.** `bits/fsk-bursts/<emitter>` (T-037b) is still published once
  per chain at detach and finished at once, so it is only useful to in-process sinks; external
  programs use `open/bits` (optionally `emitter=<id>`), which sees the same bursts live.

## Sources

- [ADR-0004](adr/0004-stream-output-contract.md), [ADR-0003](adr/0003-process-plugin-model.md), [ADR-0010](adr/0010-language-and-licence-ledger.md)
- [C24 stream-output](capabilities/C24-stream-output.md), [C22 decoder-plugins](capabilities/C22-decoder-plugins.md)
- [docs/07 §2.15–2.16](07-data-model.md)
- [spike S3 report](../spikes/s3-web-waterfall/REPORT.md), "Notes for the architecture"
- `crates/hk-api/src/bridge.rs`, `crates/hk-api/src/http.rs`, `crates/hk-api/src/auth.rs` (T-022a), [ui/README.md](../ui/README.md)
