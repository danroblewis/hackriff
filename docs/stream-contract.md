# Stream-output contract (v1.5)

**Status:** Engineering (T-016, T-014, T-022a, T-043, T-060). Implements [ADR-0004](adr/0004-stream-output-contract.md) (PROVISIONAL) and the plugin IPC of [ADR-0003](adr/0003-process-plugin-model.md). Code: `crates/hk-stream/` (contract; re-exported as `hk_api::stream`), `crates/hk-plugins/` (plugin host), `crates/hk-api/` (WebSocket bridge and read-only HTTP endpoints, §10), `hk stream-tail` (sample consumer). Revised after the T-016/T-014 review (input-class ceiling, metadata allowlist, own-key local-only, gated spectrum cap, process groups, consumer cap), again after the independent re-probe (locality enforced per consumer, metadata policy on publishers, per-row spectrum enforcement, tighter allowlist defaults), and again after T-022a shipped the WebSocket bridge (§10 mapping and auth, §11 residuals).

One contract serves two uses:
- **External consumers** of decoded messages, bits, symbols, IQ, audio and spectra (C24, workflow step 7).
- **Decoder plugins**: their stdin data plane (§9).

## 1. Versioning

- Every stream opens with a header carrying `"schema": "hackriff.stream"` and `"version": "<major>.<minor>"`. This document is **1.4**: 1.1 (1.0 plus the optional header `audio` profile and the binary `status` record type, T-043, §12) plus the inspector streams of §14 (ADR-0011, T-089): the `frame`, `status` and `edit` message record types and the optional header `inspector` object. The optional header `stage` object (§14.4) is added with stage streams (T-088).
- **1.2 added the `presence` stream (§15, T-388)**: a new `messages` stream, additive — a reader that does not know it simply does not subscribe to it.
- **1.3 changes what that stream carries (§15, T-410, [ADR-0019](adr/0019-presence-as-an-interval-with-endpoints.md))**: presence becomes an **interval with endpoints**, so the records are `presence-start` / `presence-reopen` / `presence-end` and `presence-extension` is retired. This is **not** additive — it replaces record kinds on an existing stream — so the stream's own `message_schema` is bumped from `hackriff.presence/1` to `hackriff.presence/2` rather than the document's minor version pretending nothing moved. A contract-A consumer then sees a schema it does not know, instead of silently ignoring every endpoint. The stream's *framing* is untouched, which is why this is a minor document version and a per-stream schema major.
- **1.4 makes the detected end revocable (§15, T-413, [ADR-0019](adr/0019-presence-as-an-interval-with-endpoints.md) §6.1)**: `presence-revoke` is added, `presence.last_interval` gains `revoked_s`, and `message_schema` goes to `hackriff.presence/3`. The added kind and field are additive; the schema major is for what is not — **`presence-end` becomes provisional** for one idle gap, so a consumer that files an END as final is now wrong about a record it already understands, and must see a schema it does not know rather than be quietly mistaken.
- **1.5 lets an audio stream carry two channels (§12.2, T-874, [ADR-0015 §12.13](adr/0015-decoder-synthesis.md))**: `audio.channels` may be **2**, and each data record is then `frame_samples` sample frames of interleaved `L, R` `i16` values; two-channel status records add `stereo` and `stereo_lock_losses`. It is additive and **opt-in** — only a client that asks (`listen?…&channels=2`) ever receives two channels, so a reader that ignores `channels` keeps getting exactly the mono stream it always got. **Headers now carry the document's version:** 1.3 and 1.4 changed only the presence stream's own `message_schema`, and the header `version` stayed `"1.2"` through both; from 1.5 the header says `"1.5"`. Readers refuse only a different *major* version, so the jump is harmless — and a mono audio stream is otherwise unchanged byte for byte (every other header value, every record).
- **Minor versions** may only add:
  - optional header fields;
  - optional message-record fields;
  - new record types and flag bits.

  Readers must ignore unknown fields and skip record types they don't know.
- **A major version** changes framing or existing semantics. Readers refuse a major version they don't speak.
- **One field was renamed in 1.2, and it is still the only one (T-354).** JSON record envelopes spell the record time `t_ns`, not `t` (§5.1). A rename is not on the additive list above, so this is stated rather than slipped in:
  - **The value did not change**, only its name: the same `i64` Unix nanoseconds, in the same place. Nothing about framing or semantics moved, so nothing warranted a major bump, which readers are required to *refuse* — a disproportionate answer to a field name.
  - **Reading stays backward compatible.** Every reader in this contract accepts `t` wherever it accepts `t_ns` (`hk_stream::inspector::FrameRecord` carries `#[serde(alias = "t")]`; the reference reader and the capture frame index try `t_ns` then `t`). That is load-bearing, not courtesy: §14.7 stores decoded captures as the byte stream itself, so recordings written before 1.2 are still on disk and must still seek and re-parse.
  - **Writing is not.** A 1.2 producer emits only `t_ns`. A reader written against 1.0/1.1 that requires `t` breaks — *loudly* (a missing key, not a wrong number), which is the failure this contract prefers.
  - **Why at all.** A bare `t` holding nanoseconds is the hazard `docs/api.md`'s units convention exists to remove: nanoseconds and seconds are both plain JSON numbers 10⁹ apart, so a reader taking one for the other is out by about 31 years, and only the name can say which. These records reach the HTTP API verbatim (`GET /api/captures/{id}/frames`), where the surrounding capture object's `t_first`/`t_last` **are** seconds — two units, one body, one naming cue between them. `py/examples/hkstream.py` already called the binary header's field `t_ns`; now both halves of the contract say it.

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
| `version` | string | yes | `"1.2"` (readers accept any `1.x`; `hk_stream::STREAM_VERSION_MAJOR`/`_MINOR`) |
| `stream_id` | string | yes | Producer-chosen name, e.g. `decodes/adsb` |
| `kind` | string | yes | `messages`, `bits`, `symbols`, `iq`, `audio`, `spectrum`, `sync-search` (T-162, §14.4) or `eye` (T-161, §14.4) |
| `content_class` | string | yes | Ceiling class for every record (§6). A reader treats a missing or unknown value as `metadata-only`. |
| `source` | string | yes | Producer, e.g. `hk-plugins:readsb@0.1.0` |
| `emitter_id` | uuid | no | Emitter the whole stream belongs to |
| `provenance_ref` | uuid | no | Provenance of the underlying samples (docs/07 §2.6) |
| `bitstream_id` | uuid | no | Live Bitstream descriptor row (docs/07 §2.16) |
| `datatype` | string | iq/audio | SigMF datatype of payload elements (`ci8`, `cf32_le`, `ri16_le`, `ru8`, ...) |
| `sample_rate_hz` | number | iq/audio | Sample, symbol or spectrum-row rate |
| `center_hz`, `bandwidth_hz` | number | no | RF centre and bandwidth |
| `fft_size` | integer | no | For spectrum streams; reused as the row length for `sync-search` (candidate positions per row, T-162) and `eye` (trace points per row, T-161) stage-tap streams (§14.4) |
| `dc_excluded_hz` | number | no (T-167) | For spectrum streams: half-width, Hz, of the DC/LO-leakage notch centred on `center_hz` that the producer's detector excludes (the same tolerance `GET /api/observations` `records[].window.dc_excluded` reflects). `null`/absent when the producer applies no DC mask to this stream — additive, never a guess. |
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
{"type":"message","seq":12,"t_ns":1757774400123456789,"emitter_id":"0199…","provenance_ref":"0199…",
 "content_class":"unrestricted","gated":false,"decode_id":"0199…","decoder":"readsb@0.1.0",
 "frame_model":"adsb-df17","crc_status":"valid","identity":{"scheme":"adsb-icao","value":"a1b2c3"},
 "metadata":{"icao":"a1b2c3","df":17},"content":{"callsign":"BAW123"}}
```

**Always present:**
- `type`, `seq`, `t_ns`, `content_class` (the effective class after clamping, §6);
- `gated`;
- `metadata`, which always flows.

**`t_ns` is integer Unix nanoseconds (UTC), and the name says so.** That is the whole declaration: this document no longer relies on prose to tell a reader what unit a record's time is in, because `docs/api.md`'s units convention — Unix seconds by default, `_s` means seconds, `_ns` means integer Unix nanoseconds, no third unit — reaches these records too, verbatim, through `GET /api/captures/{id}/frames` and `POST /api/inspector/parse`. Three consequences worth stating plainly:

- **Nanoseconds here are real, but only in the binary header.** §5.2's 32-byte record header carries an exact `i64`. A JSON reader's is not exact: epoch-magnitude nanoseconds are past `Number.MAX_SAFE_INTEGER` (exact nanosecond integers run out 104 days after the epoch), so `JSON.parse` has already rounded to the nearest `f64` — about ¼ µs of resolution, the same as it would have had in seconds. Divide by `1e9` and treat ¼ µs as the floor. If you need the exact instant, read the binary header or use a BigInt-aware parser.
- **It is not `t`.** Contract 1.0 and 1.1 spelled this field `t`. Readers still accept that spelling (§1), and every recording written before 1.2 still carries it; producers emit only `t_ns`.
- **`sample_index` beside it is not a time.** It is the stream's element counter (C03), and it is exact.

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
| 1 | u8 | flags: bit 0 `GATED`, bit 1 `DISCONTINUITY`, bit 2 `OVERLOAD` (sticky tune-state), bit 3 `BURST_START`, bit 4 `BURST_END`, bit 5 `CLIPPED` (this record's own samples clipped, T-981), bit 6 `FRONTEND_EVENT` (clipped and a whole-span energy step: the front end's energy, not a signal's, T-981) |
| 2 | u16 | reserved, 0 |
| 4 | u32 | payload length. For `GATED` records this is the withheld length: lengths are metadata, and no payload bytes follow. |
| 8 | u64 | `seq` |
| 16 | i64 | timestamp of the first element, ns since the Unix epoch (UTC) — **exact**: a fixed-width `i64`, unlike the JSON envelope's `t_ns`, which a `JSON.parse` reader has already rounded (§5.1). This field's position, width and unit do not change. |
| 24 | u64 | stream sample index of the first element (sample time, C03) |

The payload holds whole elements of `datatype`. Producers cannot set `GATED`; only the gate sets it.

### 5.3 Dropped marker

When a consumer's queue was full, the next record that fits is preceded by a marker naming exactly the seqs that consumer missed: `first_seq` through `first_seq + count − 1`.
- **Messages:** `{"type":"dropped","first_seq":N,"count":M,"t_ns":<Unix ns of the first dropped record>}` (`t` before 1.2, §1)
- **Binary:** record type 2 with `seq = first_seq`, flag `DISCONTINUITY`, and the timestamp and sample index of the first dropped record. The payload is `count` as a u64 LE.
- **Gated (binary, gated spectrum only):** the same record with flags `GATED | DISCONTINUITY`. It reports rows **withheld by the egress gate** (§6, spectrum enforcement), not queue drops. Its timestamp and sample index are those of the next delivered row (the last delivered row at end of stream), so withheld rows contribute only their count. The reference reader returns `Record::Dropped(DropMarker { gated: true, .. })`.

A consumer that is disconnected (§7) sees no final marker: the connection just closes.

## 6. Content gating (egress enforcement point)

**Default: gating off (T-143).** Content gating is opt-in: unless `HK_CONTENT_GATING=1` is set (or `hk_model::set_content_gating(true)` is called), every class permits content, identities are shown in clear, and nothing below withholds or refuses anything. Classes are still derived and reported (`content_class`, `source_class`) as information only. The opt-in path is untested.

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

**Binary records.** Payloads of `bits`, `symbols`, `iq`, `audio`, `sync-search` (T-162: a caller-chosen word scored against withheld bits is an oracle over them, so it is content too, unlike `spectrum`) and `eye` (T-161: an eye row's trace centres *are* the soft symbols in order, so one column of it is the demodulated bitstream — content for the same plain reason `symbols` is) are content. On a stream whose header class forbids content:
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

Gating also covers persistence: the repository refuses content under a forbidding class (`RepoError::GatedContent`, T-002). The plugin host stores the metadata-only form instead (§9.4).

## 7. Backpressure (drop, never block)

- **Per-consumer queue.** Each consumer has a bounded byte ring (`PublisherConfig::queue_bytes`, default 8 MiB), allocated once when it subscribes.
  - A record that doesn't fit is dropped for that consumer only.
  - The drop is counted, and a marker (§5.3) follows.
  - The producer never waits for socket I/O.
- **End of stream.** When the publisher finishes, consumers drain their queues; drops just before the end still get their marker. A consumer still draining after `drain_timeout` (default 5 s) is closed (`CloseReason::DrainTimeout`). The drain is already bounded — a draining consumer is offered no further record, so at most one queue is left to write — and `drain_timeout` bounds only a **write that may never return**; it is not lengthened to make the loss rarer.
- **A discarded queue is countable (T-465).** Closing a consumer for any reason frees its queue and what was in it is never written. The bytes are `bytes_discarded`, but the publisher cannot count the *records*: a queue holds framed bytes, a pop can split a record, and one discarded drop marker stands for many records. So a consumer that keeps books (`subscribe_recorder`) is handed its **final counters** with the close reason and closes them itself: `lost = (records_enqueued + records_dropped) - (records it parsed + drops it read from markers)`. `hk_store::decoded` adds that to a capture's `dropped_records`, so `frames + dropped_records` is what was published however the capture ended. Silent loss is the defect; a counted loss is a measurement.
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
    "bandwidth_hz": {"max": 2.4e6},
    "ready_signal": true                 // the plugin sends a `ready` line (§9.3); default false
  },
  "output": {
    "format": "ndjson",
    "schema_id": "hackriff.adsb/1",
    "content_class": "unrestricted"      // REQUIRED ceiling, enforced by the host
  },
  "restart": {"backoff_initial_ms": 200, "backoff_max_ms": 30000, "max_restarts": 5, "window_s": 300},
  "limits": {"input_queue_bytes": 8388608, "stall_timeout_ms": 10000, "startup_timeout_ms": 60000,
             "ready_timeout_ms": 5000,
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
- **Hang watchdog, on two budgets (T-540):** a full queue is judged against a budget chosen by what the host has *observed* of the child, not by a single constant.
  - Until the child's **first byte on stdout or stderr** — the only evidence the host has that it reached its first instruction — the budget is `startup_timeout` (default 60 s), counted as `startup_kills`. A freshly linked binary really can be spawned and then execute nothing for ~30 s (measured, T-493), and killing it for that kills a healthy decoder on its first launch after a rebuild or install.
  - From that first byte on, the budget is the much tighter `stall_timeout` (default 10 s), measured **from the byte** (or from when the queue filled, whichever is later) and counted as `stall_kills`. Detection of a genuine hang is not slowed down by the startup budget.
  - `startup_timeout_ms` must be `>= stall_timeout_ms`. Either kill names its budget in the log ring, in `PluginStats::last_hang` and in `last_exit`; neither is reported as a generic "hang". A queue that is **not** full is not evidence either way, so a quiet plugin whose producer keeps up is never killed by either budget.

### 9.3 Message plane: stdout

One JSON object per line; lines longer than `max_message_bytes` are discarded and counted as malformed. stderr lines go to a bounded log ring (`PluginMonitor::log_tail`).

| `type` | Fields | Stored as |
|---|---|---|
| `decode` | `sample_index`, `frame_model` (default `output.schema_id`), `crc_status` (`valid`/`invalid`/`corrected`/`no-crc`/`unknown`, default `unknown`), `identity` `{scheme, value}`, `metadata`, `content`, `content_class` | `Decode`. `decoder_id`/`version` come from the manifest; `demodulation_ref`/`recording_ref` from the plugin context. |
| `annotation` | `value` (label, required), `kind` (`label`/`correction`/`ground-truth`), `confidence` (0–1, default 1), `metadata`, `content`, `content_class` | `Annotation`, author `decoder`. The target is the context's detection, else region, emitter or recording. |
| `log` | `msg` | Log ring |
| `ready` | — | Nothing stored; marks the plugin ready for input (see **Readiness**). |

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
- **Example paging policy (not a plugin):** `crates/hk-plugins/policies/restricted-paging.json` (`hk_plugins::EXAMPLE_RESTRICTED_PAGING_OUTPUT`). It allowlists only `capcode` (digits, at most 8), `function` (enum `0`–`3`), `baud` (enum `512`/`1200`/`2400`) and `encoding` (enum `numeric`/`alpha`/`tone`). The record's `t_ns` is host-stamped from `sample_index`. Message bodies, numeric pages included, are content and are never allowlisted.

**Logs:** plugin `log` lines and stderr are stored in the log ring only when the ceiling permits content; otherwise they are counted (`log_lines_withheld`, `stderr_lines_withheld`). Host errors about malformed lines name the field, never the offending value. `PluginMonitor::log_tail()` returns the lines tagged with the ceiling, so a control API can gate them.

**Readiness (T-223).** `PluginState::Running` only means the process is attached: a decoder can still be setting up what it needs to account for input. A manifest that sets `input.ready_signal: true` promises a `{"type":"ready"}` line once it is past that, and the host tracks it (`PluginStats::ready`, `PluginInstance::wait_ready`, and `records_offered_before_ready`, the records offered before the first `ready`). A producer that can pause — a lossless replay, whose gate cursor holds capture — must hold its first record until then; a producer that cannot (a live chain) waits at most `limits.ready_timeout_ms` (default 5 s) and then feeds anyway, counting it. **That bound runs from the child's first byte of output, not from the spawn** (T-629), on the same two-budget rule as the hang watchdog above: a `ready` line *is* output, so a child that has produced nothing has not got as far as the thing being waited for, and until its first byte the longer `startup_timeout` governs. A decoder that runs and then fails to signal is still caught after exactly `ready_timeout_ms`. The expiry says which case it was — ran-but-never-signalled, or never-produced-a-byte — because they are different faults. A restart re-arms the flag: the new process reports ready again. Without the declaration a plugin is ready as soon as it is attached, and nothing waits. The `ready` line carries no values, so it is accepted under every class.

**Early-input accounting is per process (T-224).** `records_offered_before_ready` counts each record as it is offered while the running process has not reported ready, not a snapshot of the cumulative `records_offered` taken at the `ready` line: `records_offered` spans the instance while readiness re-arms at every restart, so a snapshot would charge a restarted process with every record the instance ever offered. A process that never sends its line — the live case below — therefore counts every record fed to it, and `== 0` means what it says: no record reached a decoder that had not accounted for itself.

**The live bound is a sample-loss budget.** A chain waiting for readiness is not reading the ring, so up to `ready_timeout_ms` after the decoder's first byte (plus whatever the OS spent getting it there) of live samples can lap out of it and be counted as lost (the same is true of the 15 s wait for `Running` that precedes it). Choose `ready_timeout_ms` from the decoder's measured start-up, not from the largest wait that seems harmless. A producer's wait also ends immediately on shutdown, so a plugin that declares readiness and never signals cannot hold a chain past a stop or detach.

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
- **End of input (T-103):** `PluginInstance::finish(idle)` ends a plugin's input without losing it. Records already queued are still written, then the plugin's stdin is closed (EOF), and the host waits for the plugin to flush and exit on its own; that exit is not restarted. A plugin still starting up (not yet reading stdin) is waited for as well. It is killed only after `idle` without progress (input consumed, lines stored, state change) — and `idle` runs on the same two budgets as everything else here (T-629): before the child's first byte of output, "nothing has happened" is no evidence about the child at all, so the longer `startup_timeout` governs; from that byte on, `idle` runs from the later of the byte and the last progress. A decoder must therefore flush its output and exit at stdin EOF. The pipeline's plugin chain calls it at detach instead of a fixed settle window after the last record, which cut off slow-starting plugins (0 decodes under load).
- **Limits:** only `nice` (via `setpriority`), the queue size, message size and log ring are enforced. Memory and CPU caps are future work.

### 9.6 Fit for readsb (T-015)

readsb is GPL, so it will run as a subprocess against this manifest. Its model:
- `input.kind: "iq"`, `datatype: "ci16_le"` (readsb `SC16`) or `"cu8"` (`UC8`), `framing: "raw"`, `sample_rates_hz: [2400000]`, `center_hz` around 1090 MHz;
- readsb reads the raw samples from stdin (ifile device with `/dev/stdin`);
- its JSON/raw output must become §9.3 `decode` lines, `identity {scheme: "adsb-icao"}`, `crc_status: "valid"`. readsb doesn't print per-message NDJSON on stdout by default, so T-015 will likely need a thin wrapper or a network-output adapter;
- `content_class: "unrestricted"`.

**Readiness bound (T-223/T-224).** `hk-plugin-readsb` reports ready once readsb's Beast connection is up and the pre-roll is written; under load at `nice` 10 that took 1.63 s, and the wrapper itself gives up waiting after 25 s and reports ready regardless, so its output is never withheld. The manifest sets `limits.ready_timeout_ms` explicitly to **5000**, the same value as the default, because the number is a decision and not an accident: it is ~3× the measured worst case, and it is what a live chain pays in lapped ring samples when a decoder never signals. Raising it towards the wrapper's own 25 s bound would trade seconds of live coverage for a start-up case that has never been observed; lowering it towards the measurement would feed readsb unready under ordinary load. A lossless replay ignores the bound (it waits as long as a lossless push would, holding capture with its gate cursor), so this value only ever governs live chains. Both forms start at the wrapper's first byte (T-629), so neither is a race against the dynamic loader on a freshly linked binary — which is what made `signal_001`'s `plugin_fed_before_ready == 0` red under load, on gates that relink.

### 9.7 Test plugin

`hk-dummy-plugin` (bin target of `hk-plugins`, manifest `plugins/dummy/manifest.json`) reads framed or raw input and emits one decode per `--every` records. Its test switches:
- `--profile adsb-like`
- `--crash-after K`
- `--ready-after-ms MS` (the §9.3 `ready` line after a start-up delay; omitted with `input.ready_signal` set, the plugin never signals)
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

**A `stream_id` outlives its publishers (T-417).** A retune finishes the spectrum publisher and
offers a new one under the **same id**, because a header must describe every row after it (T-057);
a re-plumb rebuilds every reader around the still-open device (T-399) and does the same. Neither is
a reason to drop the browser — the user, 2026-09-17: *"a settle gap is fine … so connected
consumers keep receiving after the retune; and the UI spectrum websocket client should reconnect
gracefully across a re-plumb rather than erroring."* So on the bridge:

- the socket **stays open** when a publisher finishes, and the connection is re-subscribed to the
  next offer under that id (`bridge::watch_peer`, up to `bridge::CARRY_OVER_GRACE` = 60 s, after
  which the connection ends as it always did);
- that publisher's header goes out as **another text message** on the live connection, and the
  records after it belong to it. A client reads any later text message whose `schema` is
  `hackriff.stream` as a new header — no record carries `schema`, so the two never collide;
- **the gap stays visible.** Between the last row of the old window and the first of the new,
  nothing is sent: no held frame, no repeated row, no interpolation. The capture clock skips,
  because the front end really was moving, and the header is the honest seam. (The coverage rule —
  grey means genuinely unobserved — applies to time as much as frequency.)
- **and the gap is only the gap (T-425).** The re-subscribe waits on the registry
  (`StreamRegistry::wait_for_offer_after`), not on `bridge::WATCH_TICK`, so it happens within
  microseconds of the offer. The producer offers the next publisher when the new segment's first
  samples arrive and publishes that window's first record a row period later (~40 ms), so a
  tick-quantised re-attach silently swallowed the first one or two rows of every new window — with
  no drop marker, because the consumer was not subscribed to be told. That made each retune look
  like a longer break in the air than it was: the same lie as papering the seam over, told in the
  other direction. A stalled consumer can still lose rows; what is fixed is losing them by design.
- **this is not §7's drop policy.** `SlowConsumer`, `PeerGone`, `DrainTimeout` and `Detached` all
  shut the socket down exactly as before: a consumer that cannot keep up is still dropped
  deliberately, never the survey. Only `PublisherFinished` is carried.

**The framed transports are unchanged.** Over TCP/UDS (§§2–7) a header is still sent once per
connection and a consumer of an always-on stream ends when its publisher finishes; the carry-over
is a property of the §10 bridge mapping, where each record is already a self-delimiting message.
Extending it to the framed transports would change what `StreamReader` and the plugin host must
parse, and is a contract change, not a bridge one.

**Locality: always remote.** Every browser connection subscribes wrapped in `Declared::remote`,
**even from `127.0.0.1`** — a page can forward what it receives, so it is treated as remote-capable
regardless of where the socket originates. Consequently an `own-key-decrypted` stream is refused
before anything is queued (`StreamError::LocalOnly`, `gate_stats().remote_consumers_refused`),
answered as **HTTP 403** with no WebSocket upgrade (the `101` response is written by the sink only
after `subscribe` succeeds, so a refusal never upgrades the connection). Every other gate (class
clamping, gated-spectrum rate/size, §6) applies unchanged; the bridge adds no gating of its own.

**A consumer that arrives *during* the gap is carried too (T-530).** The carry-over above is for a
connection that is already attached; one that opens while the producer is between publishers used
to be told the stream had finished. It has not: a producer that will offer a successor under the
same id says so (`Publisher::finish_between_windows`), a subscription refused in that state is
`StreamError::BetweenWindows` rather than `StreamError::Finished`, and the `/ws/<id>` handshake
**waits up to 2 s for the successor and upgrades on it** — the same wait `watch_peer` already does,
for the same reason. Past that bound it is refused **`503 replumbing`** (`Retry-After: 1`), which
means *not now*; past `BETWEEN_WINDOWS_GRACE` (5 s) the promise has failed and it is
`StreamError::Finished` again. **`410` therefore still means the stream is really over** — a
producer that is done calls plain `finish` — and that distinction is the point: it is what stops a
re-plumb reading as a dead server to anything health-checking the handshake.

**A refused subscription never touches the caller's transport** (T-530). `subscribe` checks
admission twice — once before the consumer exists, once after its writer thread is spawned — and
the second refusal used to run the caller's `closer`, i.e. shut down the very socket the refusal
was about to be written to. A `/ws` handshake that lost that race (the publisher finished between
the two checks — exactly what a re-plumb does) saw an aborted connection instead of an answer.

**Other refusals, also plain HTTP before any upgrade:**
- **HTTP 503** at `PublisherConfig::max_consumers` (`StreamError::TooManyConsumers`), and
  `503 replumbing` between windows (above);
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
within `request_timeout` (default 10 s), and connection threads run in **two capped pools**
(T-1063): at most `ServerConfig::max_connections` (default 256) HTTP ones and
`max_ws_connections` (default 128) WebSocket ones, a connection moving from the first pool to the
second as soon as its handler sees a `/ws/…` `GET`, so long-lived stream sockets cannot exhaust the
HTTP slots. Past a cap the connection is **answered** `503` with `Retry-After: 1` and
`{"code": "overloaded"}` — a client should retry — and counted on `GET /api/health`; before
T-1063 it was dropped unanswered, which behind a tunnel reads only as EOF. `/api/history` and `/api/floor` (below) additionally cap query
result size.

**Consumers never write.** As in §2: any byte a browser sends (a close frame included) or a hang-up
is read by `watch_peer` as the signal to close that consumer and shut its socket down. `watch_peer`
polls the socket on a short read timeout rather than blocking forever, so it can also notice that
its publisher finished and carry the connection over (above).

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
  - **Time fields:** the `t_ns` of a restricted record carries about log2(input range) bits via an in-range `sample_index`, or host arrival time for lines without one.
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

**An on-demand stream is the live edge, and has no history-window form (T-387).** `/ws/open/<name>` serves a consumer that asked to start listening *now*; it takes no `t0`/`t1`, and a caller cannot ask it about a past window. This is a deliberate boundary in the contract, not an omission:

- A stream's job is to carry what is being produced. A window *selects stored records*, which is a query, and queries belong to the control/query API, which owns the index that makes a time scrub a seek rather than a scan (§14.7).
- So where a UI surface must be a view over a past window, the window goes on the **route that already has one**. The packet inspector — the one surface here whose records are data about the air — scrubs through `GET /api/captures/{id}/frames?from_t&to_t`, not through a history form on `open/inspector` (docs/api.md "Decoded captures").
- The corollary is the honest one: a surface that *cannot* be windowed because no such data exists must **say it is live-only** rather than sit silently on the live edge looking windowed. `status` records (§14.3) are the case — decoder telemetry, stored in the capture file but indexed and served nowhere by time — so the workbench's stage-status strip, its pipelines list and the outputs dock each declare themselves live-only.
- `open/inspector?capture=<id>&from_frame=<n>` (§14.7) is a **capture replay**, not a window: it is keyed by capture and frame and paces to the end of the recording. Replaying a recording is not the same act as asking what a window holds.

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
  - `kind: "audio"`, `datatype: "ri16_le"` (channel-interleaved; mono unless `audio.channels` is 2), `sample_rate_hz: 48000`;
  - `center_hz`/`bandwidth_hz`: the demodulated RF channel;
  - `emitter_id` when an emitter was requested;
  - `audio`: `{channels, frame_samples, mode, mode_confidence, mode_rules, params, snr_db, squelch, agc, deemphasis_s, demod}`.
    - `mode` is chosen by auto-mode selection (`wfm`, `nbfm`, `am`, `usb`, `lsb`, `cw`); there is no manual mode.
    - `channels` (1.5, T-874): **1** unless the client asked for stereo **and** the demodulator can deliver it, then **2**. Stereo is opt-in: `listen?…&channels=2` (`channels=1` is the default, anything else is `400 bad-request`). Only broadcast FM (`mode: "wfm"`) has a second channel, so a stereo request on any other mode is served — and labelled — mono; the header says what the stream carries, never what was asked. Fixed for the stream's life. `max_frame_len` is `32 + 4 × 960 × channels` (a record header plus two frames), so a mono header is unchanged.
    - **Stereo is labelled by the status, not the header.** A two-channel stream keeps its shape whether or not L−R is being decoded: while the 19 kHz pilot is unlocked (absent, fading, just rebuilt after a retune) both channels carry the same mono audio, bit for bit — never an L−R guessed from an unlocked carrier — and the status record's `stereo` is `false`. Every locked → unlocked transition increments `stereo_lock_losses`, so a loss between two status records is still reported.
    - `params` is docs/07 `EstimatedParams`.
    - `squelch` is `{open_snr_db, hysteresis_db, noise_dbfs}`; `agc` is `{enabled, target_dbfs, max_gain_db}`.
    - Recipe audio (optional, additive; ADR-0011 §8.2, T-866; mono — `channels: 1` — until a recipe's `audio_out` takes two inputs): on a recipe pipeline's `audio` output (`audio/<pipeline>/<output>`) the `audio` object also carries `pipeline_id`, `recipe` (`<id>@<version>`), `output_id` and `edit_rev` (the revision the stream was offered at); its `mode` is the recipe's declared `profile.mode` with `mode_rules: "recipe-declared"` and `mode_confidence: 0` — a declaration, not an estimate. Listen's own chain omits the four keys. Readers that ignore unknown fields are unaffected.
    - `wait` (optional, T-987): present only when the stream opened **squelched, waiting for the carrier** — the probe demodulated nothing at the instant of opening (a bursty channel opened between transmissions) and the target's own history chose the mode. `{emitter_id, last_seen_ns, mode_bursts, analog_bursts, probe, statement}`: the emitter whose past bursts chose `mode` (`mode_bursts` of its `analog_bursts` analog observations named it), its latest sighting (Unix ns), the probe's own reason for demodulating nothing, and one line for a person — `waiting for carrier (last seen 2026-09-25T06:31:02Z, mode nbfm from 3 of 3 bursts)`. On such a stream `mode_confidence` is the history's agreement (`mode_bursts / analog_bursts`), `mode_rules` ends `+history`, `params` are what those bursts measured, and `squelch.noise_dbfs` is the channel's floor measured on the silence, so status records flow with `squelch_open: false` and no data records until the carrier returns. Absent when the probe recognised the carrier; readers that ignore unknown fields are unaffected. A target with no analog history is still refused `422 no-analog-mode`.
    - `refinement` (optional, T-070): present when the channel was refined from the demodulator's own output (`hk_pipeline::refine`). `{provenance: "refined by output analysis", objective, center_hz, bandwidth_hz, start_center_hz, start_bandwidth_hz, quality, converged, iterations, evaluations, elapsed_s, mode_params, labels}`. The header's `center_hz`/`bandwidth_hz` and `params.bandwidth_hz`/`cfo_hz`/`pilot_hz` are then the refined values; the start values are the selection or detection. Readers that ignore unknown fields are unaffected.
- **Data records** (type 1):
  - payload: `frame_samples` (960, i.e. 20 ms) sample frames of `channels` `i16` LE samples each — mono 1920 bytes; stereo 3840 bytes, interleaved `L, R, L, R, …`;
  - `sample_index`: audio sample *frames* (time, not interleaved values) since the stream start;
  - `t`: time of the first sample.
  - A jump in `sample_index` is a gap (squelch closed, or samples skipped to stay live), and the next record is flagged `DISCONTINUITY`. A `seq` gap is loss.
- **Status records** (type 3):
  - 32-byte header; the payload is a flat JSON object of numbers, booleans and short tokens (`policy::metadata_is_allowlist_shaped`, enforced by `Publisher::publish_status`), so no free text rides on it.
  - Audio fields: `level_dbfs` (T-966: the delivered audio's own level — a ~50 ms meter sample (`level_tau_s`) on a ~250 ms status tick, not an interval RMS, computed from the samples the type-1 records carry, after demod, AGC and the ±1 clamp, so it never reads above 0 dBFS and is the mean power over both channels on a two-channel stream, with the smoothing time constant computed per audio *frame* — 1 sample mono, `L, R` stereo — so a stereo stream keeps `level_tau_s`, not half of it; distinct from `snr_db`, which is pre-demod DDC channel power against the noise estimate. **While `squelch_open` is false, `level_dbfs` reads the documented silence floor** (`hk_demod::audio::SILENCE_FLOOR_DBFS`, -120 dBFS), never the discarded demod output the squelch withholds (T-1015): nothing is delivered while closed, so nothing is measured, and on reopening the meter restarts from the newly-delivered audio rather than resuming a value the closed period left stale), `snr_db`, `squelch_open`, `agc_gain_db`, `frames`, `squelched_frames`, `lost_samples`, `latency_ms`, `backlog_s`, sent about every 250 ms.
  - Refinement fields (T-070): `refined_center_hz` and `refined_bandwidth_hz` (the refined channel in force, absent when not refined) and `refine_updates` (background re-refinements that retuned the channel after passing the hysteresis).
  - Stereo fields (1.5, T-874; two-channel streams only, absent on mono): `stereo` (boolean: L−R is being decoded now, i.e. the pilot is locked) and `stereo_lock_losses` (locked → unlocked transitions since the stream began, including a demodulator rebuilt by an in-place retune or refinement while locked).
  - Recipe audio (T-866): the same record also carries the pipeline's per-node `<node>.<metric>` batch (§14.3 keys) on the same tick — one record, two vocabularies.
  - They take a `seq`. The reference `StreamReader` returns them as `Record::Unknown`; `record::parse_status_record` decodes them.
- **Gating:**
  - Audio payloads are content: under a class that forbids content the egress gate withholds them (§6), as for any audio stream.
  - The listen opener refuses earlier, before a ring read; see `hk_pipeline::chains::listen` for the rule. A recipe with an `audio` output runs the same rule on its channel when it starts (T-866). Restricted bands are refused whatever the source class. Unclassified content (a fail-closed `metadata-only` source without a user classification rule) is refused.

### 12.3 IQ profile (T-165, ADR-0013 §4.9 gap 8)

`open/iq?emitter=<id>` or `open/iq?f_lo=<Hz>&f_hi=<Hz>` (§12.1): the requested band's raw
channelised samples, un-demodulated. `hk_stream::iq` (profile), `hk_pipeline::chains::iq`
(producer, `IqTapOpener`/`PipelineHandle::iq_service`).

- **Header:**
  - `kind: "iq"`, `datatype: "cf32_le"` (complex `f32` LE, re then im), `sample_rate_hz` the
    channel DDC's own output rate (§14.4's `iq` port datatype, reused here for a channel that
    isn't a recipe pipeline node);
  - `center_hz`/`bandwidth_hz`: the requested band (its midpoint and width, not necessarily where
    an emitter's measurement rounds to);
  - `emitter_id` when an emitter was requested.
- **Data records** (type 1):
  - payload: one `re, im` `f32` LE pair per baseband sample of the channel;
  - `sample_index`: channel samples since the stream start (its own counter, independent of the
    DDC's internal position, which restarts at a retune — see below);
  - `t`: time of the first sample, mapped from the raw ring chunk's own time anchor through the
    tuned sample rate (not extrapolated from a fixed start time and nominal output rate, so it
    stays correct across a retune that changes the output rate).
  - A gap (a retune the DDC must rebuild around, or a live source skipping ahead to stay off an
    unbounded backlog) flags the next record `DISCONTINUITY`. A `seq` gap is loss (§5.3, §7).
- **No status records.** Unlike Listen, there is nothing estimated (level, squelch, AGC) to
  report; per-chain counters (samples, CPU, backlog, latency) are on `/api/status`
  `chain_stats[]` like any other on-demand chain (§12.1).
- **Where the channelisation runs.** One dedicated thread per open request, reading the shared
  ring exactly like a Listen chain (never the capture thread), running a `hk_dsp::Ddc` over the
  requested band and publishing its output directly — no demodulation, no probe. It exists, and
  the DDC runs, only while a consumer is attached; the run's on-demand chain budget (§12.1,
  T-071) admits it as a `Tap`-kind chain (a raw sample stream, not a demodulation chain — the
  same bucket burst taps use), costed from the tuned sample rate the DDC's input-rate filter
  stage must run at (not the requested channel's own, lower, output rate, since that stage's cost
  doesn't fall with decimation).
- **Bounds.** The requested band is capped at `hk_stream::iq::MAX_IQ_SPAN_HZ` (2 MHz) and must lie
  inside the tuned window (`409 outside-window`); a band the channeliser cannot realise at the
  tuned rate is `422 unrealisable`.
- **Gating (fail closed).** Raw IQ is content — more directly than demodulated audio, since it
  carries the RF envelope besides, not just what a demodulator extracted from it. `kind: "iq"` is
  one of `StreamKind::payload_is_content`'s kinds (§6), so the egress gate withholds the payload
  under a class that forbids content exactly as for `bits`/`symbols`/`audio`; the opener also
  gates before any ring read, using the same `hk_pipeline::chains::listen::listen_class` rule
  Listen and burst content already apply (restricted bands and restricted source classes refused
  whatever else is true; unclassified content fails closed).

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
     `crc`, `emitter_id` (at detach), `content_withheld`. Absent values are omitted, **except
     `crc` on a framed burst (T-954)**: once a sync word was located (`framed: true`) the field is
     never omitted, and reads one of `valid`, `invalid`, `unknown` (a CRC model exists but this
     frame wasn't evaluated, e.g. truncated before the CRC field) or `absent` (no CRC model at
     all) — so a reader can always tell "no check ran" from "the check hasn't been reported". An
     unframed burst still omits `crc` (there is no frame to check).
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

## 14. Inspector streams: decoder-workbench frames (1.2 draft, ADR-0011)

**Status: 1.2 (T-085 draft, T-089).** Wire types: `crates/hk-stream/src/inspector.rs`.
- Publishing (T-089): `Publisher::publish_frame` (through the §6 message gate: the publisher assigns `seq` and `gated`, clamps the class, withholds `content` and reduces metadata as §14.5 says) and `Publisher::publish_record` (`status`/`edit`, allowlist-shaped metadata only).
- Reading recordings (T-089): `hk_stream::inspector::RecordedFrames` (§14.7); `CaptureSource` is the interface a capture store implements.
- Field-map evaluation (T-089): `hk_recipe::fields::eval::Evaluator`.
- T-088 serves the records from recipe pipelines; T-092 records them.

Everything here is additive under §1:
- new message record types (`frame`, `status`, `edit`);
- optional header objects (`inspector`, `stage`).

Framing (§3), binary records (§5.2) and gating (§6) are unchanged. Decoder-workbench concepts (blocks, recipes, field maps) are defined in [ADR-0011](adr/0011-decoder-workbench-contracts.md).

### 14.1 Header

An inspector stream is a `messages` stream:
- `message_schema`: `"hackriff.inspector/1"`.
- `stream_id`: `inspector/<pipeline_id>/<output_id>` for a live pipeline output; `inspector/capture/<capture_id>/<n>` for a replay or re-parse of a recorded decoded stream.
- `content_class`: the pipeline's effective class, `clamp(recipe output_policy.content_class, source class)` (ADR-0011 §2.6).
- `source`: `hk-pipeline:recipe:<recipe_id>@<version>`.
- `center_hz`/`bandwidth_hz`: the target channel. `emitter_id` when the target is an emitter.
- `inspector` (optional, 1.2): `{pipeline_id, recipe_id, recipe_version, output_id, source, channels}`.
  - `source` is `{"kind": "live"}` or `{"kind": "capture", "capture_id", "reparse"}`.
  - `channels` is `[{index, center_hz, bandwidth_hz}]`: the channels known at open. `follow_hops` pipelines add more later, and every record names its own channel.

### 14.2 Frame records

One NDJSON record per frame:

```json
{"type":"frame","seq":41,"t_ns":1789300800123456789,"content_class":"unrestricted","gated":false,
 "crc_status":"valid","decoder":"recipe:rds@1","frame_model":"rds",
 "metadata":{"frame":41,"sample_index":123456789,"channel":0,"channel_hz":101300000.0,"bit_len":64,
             "recipe_version":1,"edit_rev":0,"fec_corrected_bits":0,"fit":"ok"},
 "content":{"hex":"54a8…",
            "layers":{"nodes":[{"id":0,"name":"pi","path":"pi","type":"uint","bits":[0,16],"bytes":[0,2],
                                "value":21672,"text":"0x54A8","label":"Programme identification"}, "…"],
                      "byte_index":[[0],[0],[1,2,3,4],"…"],"fit":"ok"}}}
```

| Field | Meaning |
|---|---|
| `type`, `seq`, `t_ns`, `content_class`, `gated` | As §5.1. `t_ns` is the time of the frame's first bit in integer Unix nanoseconds, host-stamped from `sample_index` and the ring's time anchor. |
| `crc_status` | Frame check after FEC: `valid`, `invalid`, `corrected` (the check passed only after FEC corrected bits it cannot vouch for — usable, but never CRC-valid evidence: T-210), `no-crc` (the recipe has no check), `unknown`. |
| `decoder` | `recipe:<recipe_id>@<version>` |
| `frame_model` | The recipe id (inspector outputs) or the decode mapping's `frame_model` |
| `emitter_id` | When the pipeline's target resolved to an emitter |
| `metadata.frame` | Frame counter of this output, from 0. A gap means frames were not produced (e.g. dropped by `drop_invalid`). Transit loss shows in `seq` and §5.3 markers. |
| `metadata.sample_index` | Source sample index (ring stream counter, C03) of the frame's first bit |
| `metadata.channel`, `metadata.channel_hz` | Channel index (0 unless `follow_hops`) and its centre |
| `metadata.bit_len` | Frame length in bits |
| `metadata.recipe_version`, `metadata.edit_rev` | Saved recipe version and live-edit revision (0 = as saved) that produced the frame |
| `metadata.fec_corrected_bits` | Bits corrected before the check |
| `metadata.fit` | `none` (no field map ran), `ok`, `partial`, `failed` |
| `content.hex` | Frame bytes, lower-case hex |
| `content.layers` | The parsed layer tree, when a `fields` block ran |

**Every metadata key is optional on the wire.** Under a class that forbids content the gate keeps only allowlisted keys (§14.5), so readers must not require any of them.

**Packing.**
- `content.hex` holds `ceil(bit_len / 8)` bytes.
- Bit 0 of the frame is the **MSB of the first byte**, and the last byte is zero-padded.
- Air bit order is resolved by the recipe before framing, so every frame, field map and stored stream uses this one convention.

**Layer tree.**
- `nodes` are in pre-order (a node's children follow it), each with:
  - `id`, `parent` (absent at top level), `name` (`name[i]` for a repeat instance), `path` (dotted from the root);
  - `type`: `layer`, `uint`, `int`, `enum`, `ascii`, `bitfield`, `flag`, `bytes`. A bitfield's named bits are child `flag` nodes.
- `bits: [offset, length]`: absolute from the frame's first bit.
- `bytes: [first, end)`: the bytes covering `bits`.
- `value`:
  - integers ≤ 2⁵³ are JSON numbers, larger ones decimal strings;
  - `ascii` is a string, a flag a bool;
  - absent for layers, `bytes` and failed fields.
- `text` is the rendered value. `label` comes from the field map. `error: true` marks where a fit error was reported.
- `byte_index[b]`: the ids of the **leaf** nodes overlapping byte `b`, in bit order.
- `fit` and `errors`: `[{path, kind, need_bits?, have_bits?}]`. `kind` is `out-of-bounds`, `bad-length`, `missing-reference`, `repeat-limit`, `node-limit` or `parity` (an `ascii` character failed its parity check and renders U+FFFD; once per field).
- Leaves in `byte_index` are nodes without children, excluding layers.
- A field that doesn't fit never aborts the frame: its later siblings are still evaluated.

**Linked selection** (inspector UI, T-090) uses only these values, with no arithmetic of its own: click a field → highlight its `bytes` (and `bits` for sub-byte precision); click byte `b` → select `byte_index[b][0]`, with repeat clicks cycling through the list.

### 14.3 Status and edit records

Inspector streams also carry two metadata-only record types. Readers that don't know them skip them (§1).

- **`status`**, one record per ~250 ms tick for the whole pipeline, every node's block-contract status readout (ADR-0011 §1.3) batched as `<node>.<metric>` keys:
  ```json
  {"type":"status","seq":57,"t_ns":…,"content_class":"unrestricted","gated":false,
   "metadata":{"sync.lock":"locked","sync.snr_db":14.2,"sync.error_rate":0.012,"sync.quality":0.93,
               "sync.items_in":118750,"sync.items_out":1130,"sync.blocks_ok":4480,
               "crc.lock":"locked","crc.error_rate":0.004,"crc.items_in":1130,"crc.items_out":1130}}
  ```
  `metadata` is flat numbers, booleans and short tokens (`policy::metadata_is_allowlist_shaped`); no free text or content ever rides on it.
- **`edit`**, once per applied hot edit (ADR-0011 §2.3), before the first frame of the new revision:
  ```json
  {"type":"edit","seq":90,"t_ns":…,"content_class":"unrestricted","gated":false,
   "metadata":{"edit_rev":3,"recipe_version":1,"applied_at_sample":123456789,"rebuilt":1,"reset":4,"field_maps_changed":0}}
  ```

### 14.4 Stage streams

Any output port of any node of a running pipeline can be opened as a stream on demand (§12.1). Opening one costs nothing until it is opened, and closing it stops the tap.

| Port type | `kind` | `datatype` | `sample_rate_hz` | Payload |
|---|---|---|---|---|
| `iq` | `iq` | `cf32_le` | port rate | complex baseband |
| `real` | `audio` | `rf32_le` | port rate | real waveform (no `audio` profile) |
| `soft` | `symbols` | `rf32_le` | symbol rate | soft values, positive = 1 (as §13.3) |
| `bits` | `bits` | `ru8` | bit rate | one byte per bit |
| `frames` | `messages` | – | – | frame records (§14.2) |

- **Records.** §5.2 data records, one per processed chunk.
  - `sample_index` is the port's element index; the record header's timestamp (§5.2) comes from the chunk's time map.
  - `DISCONTINUITY` is set on the first record after a chunk discontinuity, a `RESET` (hot edit) or lost ring samples.
- **`view=spectrum`** (`iq`/`real` ports): `kind: spectrum`, `rf32_le` dBFS/Hz rows, `fft_size` declared, at most 25 rows/s, which is inside the §6 gated-spectrum cap. The rendering reduction is server-side (the UI is a thin client).
- **`view=sync_search`** (`bits` ports only; needs `sync_word=0x…` and `sync_bits=<1..=64>`, refused 400 if missing/invalid or if the word doesn't fit in `sync_bits`, the same rule the `sync_search` block itself applies): `kind: sync-search`, `rf32_le` rows of the sync-word match score (`1 - errors/sync_bits`, so `1.0` is a perfect match) at each candidate bit position, `fft_size` reused for the row length (candidate positions per row, scaled with the bit rate so the row rate stays at most 25 rows/s, the same cap as `view=spectrum`), no RF geometry (`center_hz`/`bandwidth_hz` unset — a bit-domain row, not RF-referenced). Computed on the pipeline thread, only while a consumer is attached (T-162). **Content, not metadata** (§6): the caller supplies the word, so an ungated score would let it probe withheld bits one guess at a time; it is gated exactly like the `bits` port it reads.
- **`view=eye`** (`iq`/`real` ports only; needs `symbol_rate_bd=<f>`, refused 400 if missing/invalid or if it does not give 2..=1024 samples per symbol on that port — below 2 there is nothing *between* the instants to draw, above 1024 the window a row buffers is unreasonable): `kind: eye`, `rf32_le` rows of the **clock-recovery eye/timing diagram**, `fft_size` reused for the row length, no RF geometry (`center_hz`/`bandwidth_hz` unset — the row's axes are time-within-a-symbol and amplitude).
  - **Row layout.** 64 consecutive traces of 64 points, trace-major (trace `k` occupies `[64k, 64k+64)`). Trace `k` is the waveform around the `k`-th symbol instant of that row, resampled onto a grid spanning **2 symbol periods centred on the instant**: point `j` is the waveform at `(j − 32)/32` symbol periods from it. So point **32** is the symbol instant, where the eye **opens**, and points **16** and **48** are half a symbol either side, between instants, where it **closes**. The constants are fixed by this contract, so a reader needs no extra header field: traces per row = `fft_size / 64`.
  - **The sample instants.** Estimated per row, server-side, by the classical square-law (Oerder–Meyr) non-data-aided estimate — no training sequence, no lock, no state carried between rows — and **reported**: the record's `sample_index` is the port element index of the row's **first** symbol instant, the rest following one symbol period apart. The estimate is sub-sample and the traces are folded on it at full precision; only the reported `sample_index` is rounded to the nearest port element, since it is an integer index (at most half a sample, below the trace grid's own step at 32 or more samples per symbol). That is what lets a reader line the eye up against the same port's `view=raw` tap, and what makes the diagram answer "is clock recovery sampling in the open part of the eye?" rather than merely "what does the waveform look like?".
  - **Rate.** Whole rows are dropped once the symbol rate would push past the 25 rows/s cap (the same cap as `view=spectrum`), so a row always shows 64 *consecutive* symbols but successive rows need not be contiguous. Computed on the pipeline thread, only while a consumer is attached (T-161), at one multiply-accumulate per sample plus one fold per row.
  - **Content, not metadata** (§6), and more plainly than `sync-search`: point 32 of every trace *is* the pre-decision soft symbol, in symbol order, so slicing that one column of a row recovers the demodulated bitstream outright — no caller-chosen probe needed. It is gated exactly like the `iq`/`real` port it is folded from.
- **Header** adds the optional (1.2) `stage`: `{pipeline_id, node, port, port_type, edit_rev}`.
- **Gating.** Stage payloads of `iq`/`audio`/`symbols`/`bits`/`sync-search`/`eye` are content: withheld (`GATED`) under a class that forbids content, as in §6.

### 14.5 Gating

- **Frame records are messages.** The §6 message rules apply unchanged:
  - the record's class is clamped to the header class;
  - `content` (bytes and layers) is withheld with `gated: true` when the effective class forbids content;
  - metadata is reduced to the recipe's `output_policy` allowlist, whose shape and review rules are those of a plugin manifest (§9.3).
  - Allowlisting inspector metadata keys (`frame`, `bit_len`, `channel`, `fit`, …) for a restricted recipe is a reviewed declaration like any manifest key.
- **Consequence for restricted services.** A POCSAG recipe (`restricted-paging`) serves frames without bytes or layers unless a user classification rule vouches for the emitter (own pager).
- **`own-key-decrypted` pipelines** are served to local consumers only (§2).
- **Status and edit records** carry only allowlist-shaped metadata.

### 14.6 Backpressure and bounds

- §7 applies unchanged: per-consumer bounded rings, drop markers, disconnect after `disconnect_after`. A pipeline never waits on a consumer.
- A frame record is at most `max_frame_len` (default 1 MiB).
- A layer tree has at most 4096 nodes (`hk_stream::inspector::MAX_LAYER_NODES`); past that it is truncated with a `node-limit` fit error.

### 14.7 Recorded decoded streams and re-parse (T-092, T-089)

- **Storage format.** Always-on decoded capture stores each pipeline output as a file holding **the §3 byte stream itself**:
  - the header frame;
  - optional 1.2 header key `recipe`: the full recipe document at the starting revision;
  - then the frame, status and edit records as published, but without `content.layers` (layers are derived and re-computable).

  Hot edits during a capture appear as `edit` records. `StreamReader` reads a capture file exactly as it reads a socket.
- **Content rule (§6 persistence).**
  - A frame whose effective class forbids content is stored in its gated form, metadata only. Its bytes are never written, so it cannot be re-parsed, by design.
  - Captures are quota-managed like output recordings (T-061).
- **Re-parse.** Read the frame records, evaluate a field map (the recording's own or an edited one) over `content.hex` and `metadata.bit_len`, and re-emit them with layers.
  - The re-emitted records carry `inspector.source = {kind: capture, capture_id, reparse: true}`.
  - `metadata.recipe_version`/`edit_rev` stay the recording's, so a record always says which pipeline revision produced its bytes.
  - It is served paged over HTTP (`POST /api/captures/{id}/parse`, docs/api.md "Inspector", with a fit summary over the whole recording) and, once the inspector opener lands (T-088/T-092), as a stream (`open/inspector?capture=`).
- **Reader.** `hk_stream::inspector::RecordedFrames::open(reader)` reads the header (a `messages` stream whose `message_schema`, if present, is `hackriff.inspector/1`) and yields the `frame` records in order, skipping and counting other records. A capture store exposes recordings by id through `hk_stream::inspector::CaptureSource::open(id) → Read`.
- **Recipe tails over recorded bits or symbols.** Running a tail over recorded `bits`/`soft` uses the T-061 output recordings as a recipe `input.port` of `bits`/`soft`.

### 14.8 Planned openers (names only; T-088, T-089)

Served over `/ws/open/<name>` (§12.1) and TCP `open/<name>` (§13.1), with the usual refusals: `404` unknown pipeline, node, port or capture; `409` a view the port type doesn't support; `403` gate; `503` budget.
- `inspector?pipeline=<id>[&output=<id>]`: a pipeline's inspector output from now. The same records are offered as the always-on stream `inspector/<pipeline_id>/<output_id>` while the pipeline runs.
- `inspector?capture=<id>[&from_frame=<n>][&field_map=<recipe_id>@<version>:<map_id>]`: replay, optionally re-parsed, of a recorded decoded stream. **Served (T-092):** frame records from frame `n` by index seek, paced to the consumer, stream id `capture/<id>`; see docs/api.md "Decoded captures".
- `stage?pipeline=<id>&node=<node>[&port=<port>][&view=raw|spectrum|sync_search|eye]`: a stage stream (§14.4).

## 15. The presence stream: interval endpoints (1.3, T-388 → T-410)

**Stream id `presence`** (`/ws/presence`), `messages` kind, `message_schema` **`hackriff.presence/3`**,
`content_class` `unrestricted`, listed by `GET /api/streams`. Published by `hk-pipeline`
(`crates/hk-pipeline/src/presence.rs`) from the detection writer thread.

### 15.1 Two contracts, and which one this is

T-388 built this stream under **contract A — presence is an accumulation of observations**. A box's
top was the newest *measured* end, and a record per open emitter per tick pushed it forward. It
existed because a box top sat 5–10 s behind the live edge: two lazy links in series, the 5 s live
offer (`LIVE_OFFER_NS`) and the 5 s `GET /api/inventory` poll.

T-410 replaced it with **contract B — presence is an interval with endpoints** (ADR-0019). The box
runs from its start **straight to the live edge** and caps only on a real detected END, so the
measurement is the START event plus the **absence of an END**. The stream therefore carries
**START / END / REOPEN** and **never a per-poll presence bump**; a continuing interval publishes
nothing at all, so over a band of steady carriers this stream is silent.

**The 5 s offer and the 5 s poll are unchanged** and still do their jobs — creating the row,
arbitrating, merging, confirming it, and serving a *window*. What changed is that the poll may now
**cap** a box as well as extend one: it is the backstop for a lost END (§15.5).

### 15.2 What a record may say

**The stream says what was measured, never what is presumed.** An END carries the *measured* end
(`hk_detect::LiveExtent::t_end_ns`, the tracker's `t_last_end`) — the end of the last burst the
detector saw, never the instant the end was decided and never a clock read. So a box that has been
running to the live edge **retracts** to the truth when the END lands, rather than stopping wherever
the assumption had reached.

The presumption lives entirely in the renderer, where it is **drawn as presumption**: the span from
the last measured end to the live edge is the box's *open cap*, drawn lighter with a rule where
measurement stops, and it grows visibly as the silence grows (`ui/src/timebox.ts`,
`ui/src/waterfall.ts`). This is the same rule as `Coverage::of` (T-368) refusing to spell "never
looked" as "looked and it was quiet", moved onto the time axis.

**The end detector.** An interval closes after one idle gap of **observed** silence — the rule
`hk_model::presence` owns, for the reason it owns: *a gap shorter than the revisit period is not
evidence of absence, because the receiver was not listening.* `LiveExtent` carries that silence in
two clocks (observed and wall), so the producer can tell the two cases apart:

| the receiver | gap | closes after |
|---|---|---|
| never looked away (observed silence accounts for the wall silence) | `MIN_IDLE_GAP_S`, 1 s | **1 s**, plus ≤ one tick |
| looked away (a sweep, a retune) | `MAX_IDLE_GAP_S`, 60 s | 60 observed s — in practice the 5 s poll closes it first |

On a live dwell — the whole of Explore — the stream and the poll close at the same instant, because
both are `now − t_end > 1 s` on the same timestamps. On a sweep the poll closes first. **The stream
can be late; it can never be the reason a box stays open.**

### 15.3 Record shape

```json
{"type":"message","seq":7,"t_ns":1757774400123456789,"emitter_id":"0199…",
 "content_class":"unrestricted","gated":false,"frame_model":"presence-end",
 "metadata":{"kind":"presence-end",
             "last_interval":{"t_start_s":1757774390.1,"t_end_s":1757774400.12,"open":false,"revoked_s":0.0}}}
```

- `metadata.kind` and `frame_model` are one of **`presence-start`**, **`presence-reopen`**,
  **`presence-end`** or **`presence-revoke`**. `presence-extension` is retired, and the schema bump
  to `/2` is what stopped a contract-A consumer silently ignoring every endpoint on the stream; the
  bump to `/3` (T-413) is because **`presence-end` changed meaning** — it is now provisional for one
  idle gap, so a consumer that files an END as final is wrong about a record it already understands,
  which is not something an added kind alone would have told it.
- The envelope is §5.1's, unchanged. `t_ns` is the instant the record is *about* — the interval's
  start for an opening record, its measured end for a closing one — integer Unix nanoseconds, never
  a bare `t` (§1, T-354).
- `metadata.last_interval` is deliberately the **same object** `GET /api/inventory` serves as
  `presence.last_interval`: the same field names in the same `_s` seconds unit (`docs/api.md`), so a
  client assigns it rather than converting it and the fast surface cannot invent a shape the slow
  one would disagree with. That includes `revoked_s` (T-413) — measured silence inside the interval
  whose end was revoked.
- `open` is `false` only on `presence-end`. It is stated on the wire, never inferred on the client,
  and a record whose `open` disagrees with its `kind` is malformed.
- **No frequency.** An endpoint is new *time*, not new geometry (T-362: a box is a band fraction
  plus two absolute capture times). The box's edges came from the row and are not restated, so this
  path can never move a box sideways.
- `content` is never present: there is no content, only timing — of exactly the class
  `/api/inventory`'s own `presence` object already carries unconditionally.

### 15.4 START, REOPEN, REVOKE, and which tracks are published

**START versus REOPEN is an identity question; whether there is a new interval at all is an absence
question.** Neither threshold is picked:

```
silence ≤ idle gap                          → the same interval continues; NO RECORD
idle gap < silence ≤ 2 × idle gap           → presence-revoke: the END is withdrawn, ONE interval
silence > 2 × idle gap, same emitter        → presence-reopen: a new interval, a SECOND box
silence > 2 × idle gap, no existing emitter → presence-start
silence > 60 s                              → never revocable: the tracker already closed the track,
                                              so the discontinuity is a measurement (§15.2)
```

**`presence-revoke` (T-413): a detected end is provisional.** The user's ruling is that a signal
resuming within tolerance **nulls the end and keeps the one interval open**, on the same row, rather
than splitting it or spawning an emitter — and the tolerance is the **existing idle gap**, with no
new parameter. The window is anchored on the END *event*, not on the measured end: an END only fires
once a full gap of silence has been observed past the measurement, so a window measured from the
measurement could never be reached. One gap from the decision puts its far edge at
`measured end + 2 × gap`, which is the form both this stream and `hk_model::presence` are written in
(`IdleGap::revocable_nanos`) — the same predicate on the same timestamps, so the fast and slow
surfaces cannot disagree about which resumptions are one interval. **So the effective join tolerance
is two gaps although only one constant exists**: the first is the observed absence that justifies the
end, the second the observed absence that confirms it.

A REVOKE carries the interval's **original** `t_start_s` — that is what distinguishes it from a
REOPEN: one box grows, rather than a second appearing — and a `revoked_s` that is a measured **lower
bound**, the silence the receiver watched before deciding. The resumption's own start is not in a
`LiveExtent` (it carries `t_first`), so the stream states what it can show and the 5 s poll states
the whole gap. **`revoked_s` is never air**: `on_air_s` and `duration_s` both subtract it, so a join
across measured silence changes how many *events* were seen and never how much air was claimed —
without which the rejoin would be the ADR-0017 hull pathology one level down.

The gap is the same constant, derived the same way, as the one that closes an interval (§15.2). A
REOPEN never stretches a box across the silence — it replaces `last_interval`, so the returning
signal gets its own box and the gap between them is drawn as a gap. The two kinds render identically;
the label exists so a consumer can tell a returning emitter from a new one without re-querying.

Endpoints are published only for tracks the inventory has already given a row
(`Inventory::emitter_of_track`). **A track with no row publishes nothing**: a record names an
emitter, and a track with no row has no box. The set of *extents* is wider than the set of live
*offers* — an offer decides whether to *create* a row, so it is strict and excludes exactly the
continuous carriers a broadcast band is full of; an extent creates nothing, so the row lookup is the
only gate it needs.

A track that closes, merges or joins a hop set **publishes its END**: an emitter this stream has
announced open and then sees no extent for is ended at the last measured end it was announced with.
That is what makes "there is no path that produces no END" true — and it is suppressed when the
extent batch was at its cap, since absence from a truncated list is not evidence a track closed.

**A track that returns after its own END, and outside the revocation window, publishes nothing
more.** The tracker joins bursts across
its `idle_timeout_s` (60 observed s), far past the gap a box caps at, so the same track reappears
after the END; but its extent carries `t_first`, the track's *first* burst, not the resumption.
Publishing that as a new interval's start would claim the silence the END was just drawn for, so the
stream stays quiet and the 5 s poll serves the new interval with the start it actually has. A **new
track** bound to the same emitter is different, and does publish — a `presence-revoke` inside the
revocation window, a `presence-reopen` outside it — because its `t_first` is its own first burst,
which is a correct interval start and an exactly-known gap. That is what makes the reopen/new-start
distinction the **tracker's** continuity judgement rather than a threshold chosen on this stream.

### 15.5 Rate, truncation, and the backstop

Two producer-side gates, unchanged:
- at most one tick per **250 ms** of stream time (`PRESENCE_PUSH_NS`), and
- at most **32** records in a tick (`MAX_EVENTS_PER_TICK`).

That is a hard ceiling of **128 records/s**, however busy the band is, because the cap is on records
and not on tracks.

**The truncation policy changed with the contract.** Under A a record left out of a tick cost only
freshness, so dropping it was right. Under B a dropped END costs an over-claim of silent air — so
within a tick **ENDs are published before STARTs and REOPENs**, and what does not fit is **carried to
the next tick** rather than discarded (counted as `/detect/presence_extensions_truncated`). §7's
backpressure is otherwise unchanged: a slow consumer is dropped, never the survey.

**The backstop, and the bound it puts on a missed END.** A consumer that loses an END — dropped as
slow, socket closed, paused — is corrected by the next `GET /api/inventory`, which serves
`presence.last_interval` with `open: false`. So the worst a viewer sees is a box over-claiming **≤ 5 s**
of silent air, against **≤ 1.25 s** typically. This works only because both surfaces close on the
same measured gap (§15.2): a poll that still read `open` for 60 s would re-open every box the stream
closed.

### 15.6 Consumers

Only a **following** view subscribes. A paused or scrubbed view is answering about a fixed past
window; every interval's endpoints there are already known and served by the poll, so the socket is
closed and that path stays on the poll (`ui/src/app/explore/presence-stream.ts`). The client applies
a record to a row it already holds, and refuses (`ui/src/presence.ts`, ADR-0019 §7):

- **no interval on the row** — nothing to cap or open, and none is conjured; rows are created by the
  poll, never by the stream;
- **a REVOKE addressed to a later interval than the one held** — a revocation re-opens the interval
  it capped, so one claiming a later `t_start_s` is not about this row's interval;
- **anything that would shorten the measured extent** — an END at or before the end already held, an
  opening record that would move a live interval's start later, or a REVOKE that would pull the
  measured end backwards. Capping the open cap is not
  shortening: that span was assumption standing in for a measurement.

T-388's third refusal — *a span starting after the end held is refused, and the box waits for the
poll* — is **removed**, replaced by REOPEN (§15.4). The silence is still never claimed; it is now
drawn as a gap between two boxes rather than hidden behind a box that quietly stopped moving.

## 16. The analyze stream: `hackriff.analyze/1` (T-859, ADR-0015 §5.2, ADR-0021 §4.3)

**On-demand opener `analyze`**, served at `/ws/analyze/{id}` (and `/ws/open/analyze?id=<id>`, TCP
`open/analyze?id=<id>`): one region-analyze job's progress (docs/api.md "Analyze"). A `messages`
stream, `message_schema` **`hackriff.analyze/1`**, `content_class` `unrestricted`, published by
`hk-pipeline` (`crates/hk-pipeline/src/synth/jobs.rs`). It is **additive** — a new opener name, a new
schema — so it changes nothing an existing reader reads, and the document stays 1.4.

- **Everything is metadata.** Each record's `metadata` is `{type, job_id, …}`; `content` is never set.
  The one content-bearing field a job has, a result's `frames_preview`, is emptied **before** the job is
  visible anywhere unless the acquired IQ's class permits content (ADR-0015 §5.3), so the stream cannot
  carry it and needs no per-record gate.
- **Records are idempotent snapshots and drop, never block** (ADR-0015 §5.2). Each subscriber has a
  bounded queue (64 records) in front of the publisher's own §7 queue; a full queue loses the record,
  never the job's time. `GET /api/analyze/{id}` is always authoritative.
- **Record types** (`metadata.type`): `progress` (`{job}`; the first record of every stream, then on
  each state change and at most once a second), `stage` (`{stage, job}` when the deepest stage reached
  rises), `best` (`{results}`, the top 3), `trace` (`{stage, tried, not_tried, by_outcome, nodes}`, once
  per stage, ≤ 8 nodes — ADR-0021 §4.3, never per decision), `done` (`{job}`, the final job).
- **Lifetime.** The stream ends after `done`. Opening it for a finished job yields exactly one `done`.
  An unknown id is refused `404 not_found`; a forgotten one `410 gone` (ADR-0021 §4.2: *we forgot* is
  not *it never ran*).

## Sources

- [ADR-0004](adr/0004-stream-output-contract.md), [ADR-0003](adr/0003-process-plugin-model.md), [ADR-0010](adr/0010-language-and-licence-ledger.md)
- [C24 stream-output](capabilities/C24-stream-output.md), [C22 decoder-plugins](capabilities/C22-decoder-plugins.md)
- [docs/07 §2.15–2.16](07-data-model.md)
- [spike S3 report](../spikes/s3-web-waterfall/REPORT.md), "Notes for the architecture"
- `crates/hk-api/src/bridge.rs`, `crates/hk-api/src/http.rs`, `crates/hk-api/src/auth.rs` (T-022a), [ui/README.md](../ui/README.md)
