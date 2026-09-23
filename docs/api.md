# API reference

**Status:** Engineering (T-050, T-051, T-052, T-060, T-061, T-067, T-078, T-079). Code: `crates/hk-api/src/http.rs` (`ROUTES`, the complete table — nothing else answers `2xx` under `/api/` or `/ws/`), `control.rs` (device/display/recording/bookmarks), `selections.rs`, `outputs.rs`, `inventory.rs` (T-078 lifecycle), `query.rs` (read-only history/floor/inventory/analysis), `ondemand.rs` (`/ws/open/*`), `bridge.rs` (`/ws/<id>`, discovery), `tcp.rs` (the TCP stream server). Contract tests: `crates/hk-cli/tests/api_contract.rs` (drives a real `hk serve` over the mock SDR device, T-049, and asserts every route's status, JSON shape and auth refusals), `crates/hk-cli/tests/control_http.rs`, and per-module tests under `crates/hk-api/tests/`.

**The web UI is a thin client over this document** (ADR-0002): the UI, `hk`, and any other program talk to the exact same HTTP/WS/TCP surface. Nothing here is UI-only. Streamed payloads (framing, headers, binary record layout, content-class gating) are the versioned wire contract in [`docs/stream-contract.md`](stream-contract.md); this document covers the plain HTTP/WS/TCP *endpoints* (status codes, request/response JSON) and links to the stream contract wherever a route opens or discovers a stream.

## Conventions

- **Base URL.** `hk serve` binds `127.0.0.1:<port>` by default (printed at start as `http://<addr>/#token=<token>`) and prints the TCP stream server's address alongside it. Binding a non-loopback address exposes every route below to anyone who can reach that interface; there is no TLS in M0 (the cloudflared tunnel the user runs separately adds TLS).
- **Units.** Frequencies are Hz. **Times are Unix seconds as JSON numbers (floats) by default, and every field that departs from that default names its unit in the field name** (T-349). So a bare name (`t0`, `raised_at`, `t_lo`, `now`) and an `_s` name (`t_s`, `duration_s`) are both seconds, while **`_ns` is integer Unix nanoseconds** (`start_ns`, `t_ns`, `generated_at_ns`). There is no third unit, and no serialized absolute time is unitless-and-not-seconds. The two units are 10⁹ apart and both are plain JSON numbers, so a field carrying one while a reader assumes the other is wrong by about 31 years: the name is the only thing that can say which, and it always does.
  - **Where `_ns` appears, and why it is not simply converted.** `hk_model::Timestamp` is nanoseconds, and the structs that carry it into these responses (`SurveyReport`, `OccupancyStat`, the observation-log records, `SignatureMatch`, `Classification`) are the **stored** records served verbatim. Their serde form is also the on-disk form of the CRC-checked observation and occupancy line logs, where the value has to survive a write/read cycle exactly — and seconds as `f64` cannot round-trip a nanosecond: at present-day Unix magnitudes an `f64` spaces **238 ns** apart (256 ns for the same instant written as nanoseconds; `f64` has 53 bits either way, so the unit barely changes it). Re-encoding them into a parallel seconds-shaped wire type would be a second schema to keep in step with the first. So they stay nanoseconds and say so.
  - **Reading `_ns` in a browser.** These integers are past `Number.MAX_SAFE_INTEGER` — exact nanosecond integers run out 104 days after the epoch — so `JSON.parse` has already rounded to the nearest `f64` and nothing is gained over seconds. Divide by `1e9` and treat about ¼ µs as the resolution.
  - **Adding a field:** if it is a time and it is not seconds, its name ends in `_ns`. `crates/hk-cli/tests/api_contract.rs::every_serialized_time_declares_its_unit` walks every route's JSON and fails on a bare or `_s` field holding a nanosecond-magnitude value, or an `_ns` field holding a seconds-magnitude one.
  - **Stream *records*** use `i64` nanoseconds throughout, in the binary record header and in the JSON record envelope's **`t_ns`** — see the [stream contract](stream-contract.md) §5.1/§5.2. `/api/captures/{id}/frames` and `/api/inspector/parse` serve those records verbatim, and since stream contract 1.2 (T-354) they obey the same law as everything else here: a record's time is `t_ns` because it is nanoseconds. It used to be a bare `t`, which was the one field on this API that carried nanoseconds without saying so — and it did it inside a body whose surrounding `capture` object reports `t_first`/`t_last` in **seconds**. Readers still accept the old `t` (recordings written before 1.2 carry it); producers emit only `t_ns`.
- **Absolute capture time on every time-varying record** ([One shared time axis](#one-shared-time-axis-t-337) below, T-337). Anything the client places on a time axis — a spectrum row, a presence extent, an event, a box, a selection, a floor step, a history cell — arrives with the absolute capture time it happened at. A client never derives a record's time from arrival order, a sequence number, a row index, a declared rate, or its own request parameters.
- **Auth (bearer token).** Every `/api/*` and `/ws/*` request needs the server's token (`Token::verify`, constant-time comparison; a missing/wrong/expired token is `401` before anything about streams, the device or an id is revealed):
  - `Authorization: Bearer <token>` works everywhere.
  - `?token=<token>` works **only for `GET` requests** (browsers can't set headers on a `WebSocket` connection, so `/ws/*` and any other read needs this form to be usable from a page). A mutating request (`POST`/`PUT`/`DELETE`) carrying `?token=` instead of the header is refused `401` with a message saying so — the token must never land in a mutating URL (proxy logs, browser history).
  - `hk serve` keeps the token in a `0600` file (`hk_api::default_token_path`, `$HK_TOKEN_FILE` or `$XDG_CONFIG_HOME/hackriff/api-token`) or `$HK_TOKEN`; the UI reads it from the URL fragment (`#token=`, never sent to the server) and keeps it in `sessionStorage` for the tab.
- **CORS.** No `Access-Control-Allow-*` header is ever sent. `OPTIONS` preflights always answer `403` (so a cross-origin page cannot ride a CORS grant to smuggle the token or a JSON body). A mutating request whose `Origin` names a different host than `Host` (or `X-Forwarded-Host`, trusted only from a loopback peer, i.e. the local cloudflared tunnel) also answers `403`. Same-origin use — the UI served by this same server, directly or through the tunnel — is unaffected.
- **Methods.** `GET`, `POST`, `PUT`, `DELETE`, `OPTIONS` are parsed; anything else is `405`. A *known* path with the wrong method is `405` with an `Allow` header listing the methods it does accept. An *unknown* `/api/*` path is `404`.
- **Errors.** Every non-2xx JSON body is `{"error": "<message>"}`; every route reached through the control dispatcher (`control.rs`/`selections.rs`/`outputs.rs`/`inventory.rs` — everything except the five read-only endpoints in the first table below and `/ws/*`) additionally carries a stable machine `"code"`: `{"error", "code"}`. Error messages never echo raw request values. Common codes: `invalid` (400, malformed/out-of-range field), `not_found` (404), `unauthorized` (401), `forbidden`/cross-origin (403), `not_live` (409, device settings on a replayed recording), `device_required` (400, a device route on a run with several front ends and no `device_id` selector — T-511), `unknown_device` (404, a `device_id` selector naming no front end this run holds), `conflict` (409, a re-plumb or another operation is in progress), `refused` (409, legal/content-class gate said no), `finished` (409, the run has ended), `timeout` (504), `out_of_range` (400, a device value outside its capabilities), `unsupported` (501, the device lacks the capability, e.g. no bias tee), `not_implemented` (501, a route is defined but its engine isn't built yet, e.g. `POST /api/analyze` for a selection or band target until MAUTO's region-analyze engine lands), `busy`/`quota` (503/507, output-recording admission), `unavailable` (503, the server has no audit log / bookmark store / output recorder / etc. for this feature).
- **Audit.** Every **mutating** request to `/api/control/*`, `/api/bookmarks*`, `/api/selections*`, `/api/outputs*` or `/api/inventory/{id}*` is written to the run's audit log (`<data dir>/control-audit.jsonl`, mode `0600`) once authenticated: time, token id (never the token), peer, method, path, action name, request body, old/new values, status, result. A request that reaches the front end also carries `device: {action, id}` (T-343), so the log distinguishes the requests that changed the radio from the ones that changed the view, and says which radio. Unauthenticated mutating attempts are logged too, coalesced per client to bound disk use. **`GET` requests are never audited**, on any route. Without an audit log every mutating endpoint answers `503 unavailable`. See `crates/hk-api/src/control.rs` module docs for the exact schema.
- **Bounded resources.** At most `ServerConfig::max_connections` (default 64) connection threads at once (WebSocket consumers included); request heads ≤ 16 KiB, bodies ≤ 64 KiB, both within `request_timeout` (default 10 s); `/api/history`/`/api/floor`/`/api/inventory` cap result size (below).
- **Receive only.** No route reaches a transmit path; `transmit.available` is always `false` (C37 stays gated at the type level, not just by convention — there is no transmit operation to call).

## Read-only query routes

| Method | Path | Auth | Query | Response | Errors |
|---|---|---|---|---|---|
| GET | `/api/streams` | token | – | Discovery document (T-060, below) | 401 |
| GET | `/api/history` | token | `f_lo`, `f_hi` (Hz), `t0`, `t1` (Unix s), `max_cells`? (default 100 000, max 500 000), `max_t`?/`max_f`? (per-axis cell budgets, 1…500 000 — T-334), `format`? (`json` default, `csv`, `png`), `stat`? (csv/png: `max`, `mean`, `p_low`, `p_high`, `floor`), `source`? (16 hex digits or `unknown`), `site`? (`unassigned`, `mobile`, a site id or `unknown`) | T-017 region-over-time grid (below); T-334 `resolution` block; T-116 hackrf_sweep CSV or PNG waterfall; T-133 source/site filter | 400 invalid region/cells/format/stat/source/site, 404 no history store on this server |
| GET | `/api/floor` | token | `f_lo`, `f_hi`, `t0`, `t1`, `max_steps`? (default 1024, max 20 000) | T-021 calibrated floor-vs-time series (below) | 400, 404 no floor product |
| GET | `/api/inventory` | token | see below | T-018/T-078 signal inventory, one page (below) | 400 invalid filter, 404 no inventory store |
| GET | `/api/inventory/{id}` | token | – | One inventory entry (T-078, same shape as a list row) | 404 not_found, 503 unavailable |
| GET | `/api/events` | token | `f_lo`, `f_hi` (Hz), `t0`, `t1` (Unix s) — **required**; every `/api/inventory` filter (`state`, `status`, `tag`, `scheme`, `family`, `relations`); `limit`? (default 200, max 2000), `cursor`? (event offset, ≤ 1 000 000) | T-264 the durable catalogue of events in this region over this period, with coverage (below) | 400 invalid region/filter/cursor, 404 no inventory store |
| GET | `/api/inventory/{id}/presence` | token | `t0`, `t1`? (Unix s, together) | T-264 one emitter's presence track: every interval with its own timespan (below) | 400 invalid window, 404 not_found, 503 unavailable |
| GET | `/api/analysis/strongest` | token | `f_lo`, `f_hi` (Hz), `window_s`? (default 5, max 300) | T-079 strongest observed signal in the band over the recent window (below) | 400 invalid region/window, 404 no history store |
| GET | `/api/navigation` | token | `center_hz`+`span_hz`? (together; `span_hz` > 0), `t_cell_s`? | T-341 the achievable `(centre, span)` grid and the history tiers; with a requested state, the nearest realizable one and its live-IQ/overview claim (below) | 400 a non-finite number, or `center_hz`/`span_hz` given apart |
| GET | `/api/timeline` | token | `f_lo`+`f_hi`? (Hz, together), `columns`? (1…4096, default 96), `rows`? (1…512, default 1). **No `t0`/`t1`** — the time extent is the ring's, not the caller's | T-338 the capture window (the IQ ring's configured retention), the compressed overview waterfall drawn on it, and T-423 the record-derived per-cell coverage plane beside it (below) | 400 unknown parameter, a half-given band, or a column/row budget out of range; 404 no spectrum history on this server |
| GET | `/api/coverage` | token | `f_lo`+`f_hi` (Hz, **required**), `cells`? (1…4096, default 256), `rows`? (1…4096, default 1), `t0`+`t1`? (Unix s, together; default the capture window) | T-368 the coverage map: which front end actually sampled which frequency **and when** (T-423's time axis), so a view greys only what was **never observed** (below) | 400 unknown parameter, a missing or half-given band, a half-given window, or a cell/row budget out of range; 404 no capture window and no `t0`/`t1` given |
| GET | `/api/status` | token | – | Pipeline counters (opaque, per-build; never content) | 401, 404 no status on this server |

None of these are audited (`GET` requests never are). All are capped in result size as noted per route.

### `GET /api/streams` — discovery (T-060)

Never returns content, only stream *metadata*: every offered stream's header fields, the on-demand openers this server exposes, and the TCP stream server's address.

```jsonc
{
  "streams": [
    {
      "stream_id": "spectrum/live", "kind": "spectrum", "content_class": "unrestricted",
      "content_permitted": true, "remote_permitted": true,
      "datatype": "rf32_le", "sample_rate_hz": 25.0, "center_hz": 100800000.0, "bandwidth_hz": 2400000.0,
      "fft_size": 1024, "dc_excluded_hz": 15000.0, "open_consumers": 0,
      "ws_path": "/ws/spectrum/live", "tcp_target": "spectrum/live",
      "format": { "framing": "u32-le length-prefixed frames; first frame is the JSON header",
                  "records": "32-byte binary record header + payload (type 1 data, 2 dropped, 3 status)",
                  "record_header_len": 32, "max_frame_len": 65536, "emitter_id": null,
                  "bitstream_id": null, "bit_framing": null, "audio": null, "message_schema": null }
    }
  ],
  "on_demand": [
    { "name": "listen", "ws_path": "/ws/open/listen", "tcp_target": "open/listen",
      "kind": "audio", "datatype": "ri16_le", "sample_rate_hz": 48000,
      "params": ["emitter", "detection", "f_lo", "f_hi"], "records": "…" },
    { "name": "bits", "ws_path": "/ws/open/bits", "tcp_target": "open/bits", "kind": "bits", "...": "…" },
    { "name": "symbols", "ws_path": "/ws/open/symbols", "tcp_target": "open/symbols", "kind": "symbols", "...": "…" },
    { "name": "iq", "ws_path": "/ws/open/iq", "tcp_target": "open/iq", "kind": "iq", "datatype": "cf32_le",
      "params": ["emitter", "f_lo", "f_hi"], "records": "…" }
  ],
  "tcp": { "addr": "127.0.0.1:8788",
           "handshake": "<tcp_target>?token=<token>[&param=value...]\\n",
           "refusal": "one frame {\"type\":\"refused\",\"status\",\"code\",\"reason\"} instead of the header" }
}
```

`tcp` is `null` when no TCP stream server runs. See [Streams](#streams-websocket-tcp-and-on-demand-openers) below and `docs/stream-contract.md` §10/§12/§13 for what each named stream/opener actually carries.

**`streams` is what this server is offering *now*, not everything it has ever offered (T-531).** A
stream appears when a producer registers a publisher under its id and disappears when that
publisher has **finished**, has **no open consumer**, and has not been re-offered for 60 s — the
same `CARRY_OVER_GRACE` the WebSocket bridge waits in a settle gap, so a retune or re-plumb (which
finishes one publisher and offers the next under the same id) never withdraws anything and a
connected consumer is never cut short. Past that grace the entry can serve nobody: subscribing to a
finished publisher is refused, and no consumer is left to carry over. A hard ceiling of 256
registered streams backs the rule up, evicting the oldest finished-and-unattached entries first and
never a live publisher. The listing is therefore **time-scoped**, like the Explore inventory — a
40-minute sweep across 6 GHz meets thousands of emitters, and the `bits/fsk-bursts/<emitter>`
stream each one produced is not still on offer an hour later. A client that wants the durable
record of what was heard reads `/api/events` and `/api/inventory`, not this document. On shutdown
every id is withdrawn at once, so a bridged consumer's connection ends immediately instead of
waiting out the carry-over grace for an offer that is never coming.

`dc_excluded_hz` (T-167, ADR-0013 §4.9 gap 10) is the half-width, Hz, of the DC/LO-leakage notch centred on `center_hz` that the producer's own detector excludes from analysis (the spectrum stream's `hk-pipeline` producer sets it from `hk_detect::DcRule::default().tolerance_hz`, the same value `GET /api/observations` `records[].window.dc_excluded` already reflects). It is additive on both `/api/streams` and the stream header itself (below) and `null` when a producer applies no DC mask to that stream — never a guess. **T-524:** the LO tone's main-lobe cells at the centre of that notch (±2 bins) in every spectrum row (and in spectrum history, so tiles and the trace too) are **synthesized, not measured** — a straight dB line between the measured bins either side, replacing the LO-leakage spike — so a client that wants to mark them reads them from this field; detection runs on its own un-interpolated frames.

### `GET /api/history` — region-over-time grid (T-017, AWARE-042)

> **No web client reads this route as of T-445.** The cutover (docs/16 §8.5) retired the two
> surfaces that did — the live waterfall's review render and the Review drawer's "Spectrum grid"
> tab — for the unified surface, which reads `GET /api/tiles`. The route stays: it is
> `hk report`'s region-over-time engine, it is contract-tested, and its `t0`/`t1` semantics are
> what T-438 built `/api/tiles` on (docs/16 §8.5a F2). **Retiring it is a backend decision this
> ticket did not take** — a UI cutover is not evidence that a server route has no other caller.

A `nt × nf` grid (row-major, time then frequency) of the finest pyramid level whose cell count fits `max_cells`, over `[f_lo, f_hi) × [t0, t1)`. Unobserved cells are `null` — *not observed* is not *quiet* (C26).

```jsonc
{
  "scheme": 1, "tile_format": 3,
  "level": 0, "unit": "dbfs-per-hz", "f_cell_hz": 3125.0, "f_lo_hz": 99600000.0, "nf": 384,
  "t_cell_s": 0.1, "t0_s": 1789300800.0, "nt": 1200, "percentiles": [10.0, 90.0],
  "max_db": [-71.2, null, "…"], "mean_db": ["…"], "p_low_db": ["…"], "p_high_db": ["…"],
  "occupancy": ["…"], "occupancy_max": ["…"], "coverage": ["…"], "floor_db": [-140.1, null, "…"],
  "frames": ["…"],
  "coverage_summary": { "cells": 460800, "observed_cells": 458000, "observed_fraction": 0.99,
                        "gaps": [ { "t0_s": 1789300850.0, "t1_s": 1789300860.0 } ], "gaps_truncated": false },
  "provenance": { "frames": 12000, "suspect_fraction": 0.0, "dropped_samples": 0, "gain_changes": 1,
                  "gain_states": ["…"], "calibration": null, "calibration_mixed": false,
                  "gain_table": null, "gain_table_mixed": false, "filter": "fm-notch", "filter_mixed": false,
                  "spur_mask": null, "spur_mask_mixed": false, "cell_shape": 109.4, "cell_shape_mixed": false,
                  "steps": [ { "t_s": 1789300900.0, "changed": ["gain"],
                               "from": { "gain": { "lna_db": 16.0, "vga_db": 20.0, "amp_on": false },
                                         "calibration": null, "gain_table": null, "filter": "fm-notch", "spur_mask": null },
                               "to": { "gain": { "lna_db": 32.0, "vga_db": 20.0, "amp_on": false }, "…": "…" } } ],
                  "steps_dropped": 0,
                  "first_frame_s": 1789300800.0, "last_frame_s": 1789300920.0,
                  "origins": [ { "source": "8a1f0c3b5d2e4f60", "site": "unassigned", "frames": 12000 } ],
                  "other_origin_frames": 0 },
  "tiles_read": 4,
  "filter": null,
  "resolution": { "source": "spectrum-history", "live": false,
                  "statement": "spectrum history: measured, reduced to this tier's cells, not live IQ",
                  "served_span_hz": 1200000.0, "max_live_span_hz": 20000000.0,
                  "level": 0, "levels": 5,
                  "t_cell_s": 0.1, "f_cell_hz": 3125.0,
                  "requested": { "max_cells": 100000, "max_t": 600, "max_f": 1024 },
                  "served": { "nt": 1200, "nf": 384, "cells": 460800 },
                  "matched": false, "over_resolved": ["max_cells", "max_t"] }
}
```

#### Span-matched resolution (T-334)

**The rule, from the user (CLAUDE.md, "Time, the waterfall, and the live view", invariant 4).** *Data, timestamps and span-matched resolution are the backend's responsibility; time↔pixel mapping and view state are thin-client presentation.* The visible span is user-selectable from seconds to the full retention, and **zooming re-scales rather than truncates**: whatever span a view asks for is served whole, at a resolution matched to it, so the client never downsamples, never interpolates between the cells it was given, and never infers a timestamp. This is the thin-client rule (§"UI decision logic moved server-side") applied to the **time** axis: mapping a time to a pixel is presentation; *choosing which value represents an interval* is a measurement, and measurements are made here.

**Asking in the view's own terms.** `max_cells` bounds the response (a product), which a grid of the wrong shape can satisfy — 16 rows × 6000 columns meets the same budget as 600 × 160. `max_t` and `max_f` are the **rows and columns the view will draw**. The level chosen is the finest whose grid over the region satisfies every budget given.

**The whole span is always covered.** `[t0, t1)` is snapped *outward* to cell boundaries and returned entire; there is no clipping and no row budget that drops the oldest rows. A region that exceeds `max_cells` even at the coarsest level is `400` — a refusal, never a silently shortened answer.

**Row times are contract, not inference.** `t0_s` is the start of time row 0 and every row is exactly `t_cell_s` long, so row *k* starts at `t0_s + k·t_cell_s` and column *j* covers `[f_lo_hz + j·f_cell_hz, f_lo_hz + (j+1)·f_cell_hz)`. `t0_s` may precede the requested `t0` (outward snapping). A client computing a row's time from those fields is reading the grid the server described, not guessing one.

**`resolution`** reports what was asked for and what was served, so nothing has to be deduced from the grid:

- `source` — which tier answered, as a **detail claim** (T-334's field; T-341 made it a three-value enum, see [Live-IQ detail versus overview](#live-iq-detail-versus-overview-t-341) below). Never `"live-iq"` here: this route reads the tiered spectrum-history pyramid and only that. Its horizon is the **pyramid's** retention (tiered, lossy, byte-budgeted), *not* the IQ ring's window — `GET /api/iqbuffer` reports that one and `GET /api/timeline` sizes the scrubber from it, and the two are different lengths. `"survey-overview"` when the served span could not have fitted one capture window, `"spectrum-history"` otherwise.
- `live` — `true` only when `source` is `"live-iq"`, so a client styles the distinction without parsing the enum. Always `false` here.
- `statement` — the claim in words, rendered by the backend, for a client that shows it rather than styling it.
- `served_span_hz` / `max_live_span_hz` — the span actually served (`nf · f_cell_hz`) and the widest instantaneous bandwidth this run can produce; the two numbers `source` was decided from. `max_live_span_hz` is `null` when nothing here can say, and then `source` is `"survey-overview"` — the weaker claim, because not knowing the window is not evidence that a span fits inside it.
- `level` / `levels` — the pyramid level served, and how many the scheme has (scheme 1: 6.25 kHz × 1 s at level 0, then ×2 in frequency and ×60/×15/×4/×24/×7 in time, to 100 kHz × 1 day at level 4).
- `t_cell_s` / `f_cell_hz` — that level's cell size; the same values as the top-level fields, repeated here so the block is self-contained.
- `requested` — `max_cells` (always, defaulted) and `max_t`/`max_f` (`null` when not given).
- `served` — the grid's `nt`, `nf` and their product.
- `matched` — `true` when the served grid meets every budget asked for; the same condition as `over_resolved` being empty.
- `over_resolved` — the budgets the served grid still exceeds, any of `"max_cells"`, `"max_t"`, `"max_f"`; `[]` when all were met.

**Error direction, and why it is that way.** The ladder is discrete, so an exact match is not generally reachable. The rule is *the finest level that fits every budget*, which errs **coarser than the view, never finer**. Drawing a coarse cell across several pixels repeats one measured value — blocky, but every pixel shows something that was measured. Reducing a finer grid in the client does the opposite: it invents the value a pixel stands for, decided with no knowledge of the noise floor, occupancy or what a peak means, and produces a picture that disagrees with the backend's own view of the same span. Fewer cells than asked for is therefore the *safe* outcome and is reported as `matched`; the client replicates and must not interpolate.

`over_resolved` is the unsafe case, and it is named rather than hidden. It arises when no level is coarse enough — a `max_f` finer than 100 kHz cells can give over a wide band, or a span whose coarsest grid still exceeds `max_cells`. The client then holds more cells than it can draw one-to-one, and any single value it picks per pixel is **its own measurement**: the field exists so it can say so, narrow the span, or raise the budget, instead of reducing silently.

T-116 additions (all additive):

- `coverage` is each cell's observed fraction of its duration — and, at a rolled-up level, of its **whole time–frequency extent**: a coarse cell sums the observed seconds of the finer cells under it and divides by its own extent, so a cell half of whose frequency span was never looked at reads `0.5`, not `1.0` (T-419). `observed_s` is foldable; a ratio is not, so `coverage` is always recomputed against the level's own cell rather than averaged or maxed from the level below. `coverage_summary.gaps` lists maximal time runs in which **no** cell of the grid was observed. A gap is never reported as quiet.
- `floor_db` is the noise-floor estimate: `p_low_db` corrected for the low-percentile bias of averaged-periodogram noise (Gamma model, shape `provenance.cell_shape`). T-141: a tile whose frames had different shapes (a scheduler's short-step rows) is corrected with the bias of the Gamma **mixture** of its level-0 values, weighted by the values folded per shape over the tile (`provenance.cell_shapes`). `null` when the frames carried no noise shape, when a mixed tile recorded no per-shape counts (tile format < 4, more than 32 shapes: `provenance.other_shape_values` > 0), or when its shapes' frames covered different numbers of cells (`values`/`frames` not equal across `cell_shapes`: a cell's percentile pools only its own frames, so the tile's weights would not be the cell's); `p_low_db` stays the raw percentile.
- T-141 (additive, tile format 4): `provenance.cell_shapes` lists `{shape, values, frames}` (level-0 cell values and frames folded per cell shape; a shape within 5 % of a listed one counts under the first seen; at most 32) and `provenance.other_shape_values` counts values whose shape is unrecorded.
- `provenance` records gain table, filter/antenna port, spur-mask version and cell shape (first value plus a `*_mixed` flag) and every front-end change as a `steps` entry (time, what changed, state before and after; at most 32, the rest counted in `steps_dropped`). Cells are not split at a step — use the steps to explain level changes as provenance, not events. `scheme` is the pyramid scheme/version id and `tile_format` the tile format written.

T-133 additions (all additive; tile format 3, formats 1 and 2 still read):

- **Origins.** `provenance.origins` lists the frames behind the whole result per origin, at most 8 origins for the whole result (not per tile); frames of further origins are summed in `other_origin_frames`. An origin is `source`, the 16-hex-digit key of the source (`hk_store::history::source_key` of the frame's own provenance `device_id` — T-304: not necessarily the run's configured `device_id`, e.g. a replay whose segments carry more than one device's provenance), plus `site`, the site at the frame's sample time (`unassigned`, `mobile` or a site id; ADR-0012 §3.5). `null` means **unknown**: tiles written before format 3, frames without a site, or frames over a tile's 8-origin cap.
- **Filter.** `source` and `site` restrict the grid to the frames of that source and/or site (both given: both must match). `unknown` selects frames of unknown source or site. Each tile also keeps at most 8 origins and counts the frames of further origins as unknown, so `site=unknown` (or `source=unknown`) can include such overflow frames, and a tile that overflowed is never a whole match for a specific source or site. Tiles are not split by origin, so a cell into which frames of other origins were folded reads `null` (**unobserved**, not quiet). The exception is a cell of a coarse tile that mixed origins: it is kept when the one finer tile it rolls up matches whole, so a site change costs about one finer tile's duration of coverage. Unknown-origin (old) history matches only an unfiltered request or `unknown`.
- **Filter disclosure.** `filter` is `null` without a filter, else `{source, site, tiles_matched, tiles_mixed, tiles_other, cells_excluded, cells_from_children}`; `source`/`site` are `null` when that field is not filtered. `provenance` then merges only the tiles whose data the grid returns. CSV and PNG exports honour the filter.

**Exports.** `format=csv` returns `text/csv; charset=utf-8` in the `hackrf_sweep` line format `date, time, hz_low, hz_high, bin_width, num_samples, dB…` (UTC; dB per `bin_width`-wide bin, i.e. `stat` dB/Hz + 10·log10(bin_width); `num_samples` = largest frame count in the line; 256 cells per line; unobserved cells `nan`, fully unobserved lines omitted). `stat` defaults to `mean`. `format=png` returns an `image/png` waterfall (one pixel per cell, frequency left→right, earliest time at the top, 8-bit palette; grey = not observed; colour range 2nd percentile to maximum of `stat`, default `max`). Errors are JSON as for every endpoint.

A region too large for the cell budget even at the coarsest level is `400`.

### `GET /api/floor` — calibrated floor vs time (T-021, SPACE-050)

> **No web client reads this route as of T-445**, for the same reason as `/api/history` above: its
> one reader was the Review drawer's retired "Spectrum grid" tab. It remains the calibrated-floor
> answer for reports and for SPACE-050, and remains contract-tested.

Same region parameters as `/api/history`, `max_steps` in place of `max_cells`.

```jsonc
{
  "region": { "lo_hz": 99600000.0, "hi_hz": 102000000.0 }, "level": 0, "t_cell_s": 1.0,
  "shape": "…",
  "steps": [ { "t_s": 1789300800.0, "duration_s": 1.0, "unit": "dbm-per-hz",
               "value_db_per_hz": -140.2, "raw_p_low_db_per_hz": -139.8, "bias_db": -0.4,
               "mean_db_per_hz": -139.0, "noise_temperature_k": 420.1, "uncertainty_db": 0.6,
               "model_uncertainty_db": 0.3, "calibration_uncertainty_db": 0.4,
               "histogram_uncertainty_db": 0.2, "statistical_uncertainty_db": 0.1,
               "cells": 32, "coverage": 1.0, "level": 0, "gain_states": 1,
               "flags": 0, "flag_names": [] } ],
  "calibrated_provenance": { "…": "as in /api/history" },
  "uncalibrated_provenance": { "…": "as in /api/history" }
}
```

**Mixed cell shapes (T-141).** A run whose history rows average different segment counts (the scheduler's short steps) folds tiles of several Gamma cell shapes. Each tile decides from its own persisted record: a uniform tile's cells use their shape's bias; a mixed tile's cells use the bias of the Gamma mixture of the tile's level-0 values (weights = values folded per shape, `provenance.cell_shapes`; the percentile's CDF point is solved on `Σ wᵢ·P(nᵢ, nᵢ·x)`), so `bias_db` then lies between the components' biases. The mixture is used only when every shape's frames folded equally many values (`values`/`frames` equal): otherwise frames of one shape covered fewer or other cells (short hops beside full-span dwells), some cells' own mix differs from the tile's, and the tile gives no floor rather than a wrong one. Weights stay per tile, not per time column: columns whose shape composition differs from the tile's are corrected with the tile's mix (the step median absorbs this). A mixed tile written before tile format 4 (no per-shape counts) contributes no cells, as before; `shape` stays the product's first shape. No field changed.

### `GET /api/inventory` — signal inventory (T-018, T-078, AWARE-053/AWARE-042)

Query parameters (all optional, combined with AND): `f_lo`&`f_hi` (Hz, given together), `t0`&`t1` (Unix s, given together; a row matches when one of its **presence intervals** overlaps the window, *not* merely when its `first_seen`/`last_seen` hull straddles it — a signal seen once at 09:00 and once at 17:00 is not "on the air" all afternoon. A row carrying no interval at all, from a legacy writer, still answers from its hull. ADR-0017 §2.1. The same window also scopes each row's `presence` and `family_in_window`, below, so the list and the liveness it renders can never disagree), `state` (comma-separated `candidate`/`confirmed`/`deleted`; **default: candidate and confirmed — deleted entries are listed only when `deleted` is explicitly asked for**), `status` (comma-separated `known`/`unexpected-here`/`unknown`), `tag`, `scheme` (identity scheme), `family`, `relations` (`shown` default / `all`; T-219, below), `at` (Unix s, T-263: the caller's own live edge for a query with **no** `t0`/`t1` — it scopes each row's `presence` projection and **selects nothing**, so a scrubbed-back list that must not be time-filtered still reads the liveness its rows had then; see `presence` "Scope" below. Refused with `400` beside `t0`/`t1`, whose `t1` is already that edge), `cursor` (row offset, ≤ 1 000 000), `limit` (default 100, max 500).

```jsonc
{
  "entries": [
    {
      "id": "0199…", "state": "confirmed",
      "lifecycle": { "state": "confirmed", "previous": "candidate", "author": "auto", "actor": "trust-confirmed@1",
                     "t_s": 1789300850.0, "reason": "continuous trust-confirmed track", "reason_withheld": false },
      "recurrence": { "occurrences": 12, "appearances": 3, "span_s": 3600.0, "on_air_s": 900.0,
                      "duty_cycle": 0.25,
                      "recent": [ { "t_start_s": 1789300000.0, "t_end_s": 1789300300.0, "count": 4, "duty_cycle": 1.0 } ] },
      "explanations": [ { "rank": 1, "service": "band-plan", "label": "FM broadcast", "score": 0.92, "flags": [] } ],
      "refined": null,
      "user_band": null,
      "f_center_hz": 101300000.0, "bandwidth_hz": 150000.0, "f_lo_hz": 101225000.0, "f_hi_hz": 101375000.0,
      "first_seen_s": 1789300800.0, "last_seen_s": 1789300920.0, "count": 42,
      "presence": { "intervals": 2, "on_air_s": 364.0,
                    "last_interval": { "t_start_s": 1789300871.0, "t_end_s": 1789300920.0, "open": true, "revoked_s": 0.0 },
                    "liveness": "live", "ended_t_s": null },
      "known_status": "known",
      "status": { "status": "known", "author": "prior", "t_s": 1789300810.0,
                  "reason": "on FM broadcast allocation", "prior_ref": "band-plan/us-fm@1", "reason_withheld": false },
      "tags": [], "tags_withheld": false, "family": "wfm-broadcast",
      "family_in_window": "wfm-broadcast",   // only when t0/t1 were given; null = nothing in the window said so
      "classification": { "family": "wfm-broadcast", "confidence": 0.9, "open_set_score": 0.1,
                           "model_version": "…", "t_s": 1789300820.0,
                           "taxonomy": null, "stage": "chain", "arb_rank": 3, "coarse": null,
                           "class": null, "top": null, "entropy_norm": null, "flags": null },
      "latest_classification": null,
      "classifications": 3,
      "cluster_id": null,
      "estimated_params": { "modulation": "wfm", "symbol_rate_hz": null, "mod_order": null,
                             "deviation_hz": 75000.2, "cfo_hz": -120.5, "bandwidth_hz": 181400.0,
                             "roll_off": null, "pilot_hz": 19000.05, "t_s": 1789300920.0,
                             "source_session": "0199…", "source_recording": null },
      "identity_scheme": "rds-pi", "identity_class": "unrestricted", "withheld": false,
      "identity_value": "A1B2",
      "snr_db": 21.4, "peak_dbfs": -18.25,
      "measured": { "snr_db": 21.4, "peak_dbfs": -18.25,
                    "t_start_s": 1789300812.0, "t_end_s": 1789300813.0, "duration_s": 1.0 },
      "relation": null
    }
  ],
  "next_cursor": null, "limit": 100, "total": 214, "identity_access": "standard"
}
```

`identity_value` is present only when the row's identity is in clear (`withheld: false`); on a withheld row a status/lifecycle reason from an author who may have seen the identity is itself withheld (`reason_withheld: true`, `reason: null`).

**`lifecycle` is the transition into the current state, carrying the latest reason for it (T-403).** `state`, `previous`, `author`, `actor` and `t_s` describe the change that put the entry in the state it is in — so `t_s` is when it was confirmed, and does not move. `reason` is the **best explanation the run has reached**, which can be written later than that change: an entry is confirmed by whichever rule is satisfied first, and the rules are not satisfied at the same moment (a continuous emission's occupancy evidence is complete seconds before a demodulator can report a pilot lock, by an amount that depends on how loaded the host is). When a stronger rule later applies, its reason replaces the weaker one as a new append-only history row carrying `state == previous`, so nothing invents a state change and the recorded explanation is a function of the evidence rather than of which rule arrived first. Clients render `reason` as the current explanation and `t_s` as the time of the change; they never treat `reason` as pinned to `t_s`. Never included: decode content, fingerprints, links. No frequency lookup ever runs before detection — the inventory is populated purely from blind measurement (vision step 4); the band-plan/licence database only supplies `explanations` and `status`, ranked, never a starting point.

**`presence` (T-284, ADR-0017 TM-2/§2.3).** **When** this emitter was on the air, seen through the request's window — the object a caller reads instead of `first_seen_s`/`last_seen_s`, which are the *hull* of the presence track and never its extent (a signal seen once at 09:00 and once at 17:00 has an eight-hour hull that is 99.99 % silence; never render it as a duration). `{"intervals", "on_air_s", "last_interval": {"t_start_s", "t_end_s", "open", "revoked_s"} | null, "liveness", "ended_t_s", "silence_s", "confidence"}`:

- **`intervals`** — how many presence intervals intersect the window; **`on_air_s`** — time on air *inside* it, Σ of each interval's intersection with the window. Together they are the honest rendering: *"17 events, 4.2 s on air"*. `on_air_s` is what a live list **ranks by**, in place of the lifetime `count`.
- **`last_interval`** — the latest interval intersecting the window, or `null` when none does. It is the box drawn across trace and waterfall. **When `open` is `true` the box runs to the live edge, and `t_end_s` is where measurement stops rather than where the box ends** (T-410, [ADR-0019](adr/0019-presence-as-an-interval-with-endpoints.md)): the span between the two is the *open cap*, which a client must draw as assumption and must never present as measured air. Endpoints also arrive over the `presence` stream between polls (below), so a box caps within ~1.25 s of its emission stopping rather than on the next poll.
  - **`revoked_s`** — measured silence **inside** this interval whose detected end a resumption revoked (T-413, [ADR-0019](adr/0019-presence-as-an-interval-with-endpoints.md) §6.1). `0` for the overwhelming majority of intervals. A detected end is provisional for one further idle gap; a signal that comes back inside it nulls the end and the interval stays **one** interval on the same row, with the silence it was rejoined across recorded here. `on_air_s` and the interval's `duration_s` both **subtract** it, so rejoining never buys air time. A client with a non-zero `revoked_s` must not draw the interval as continuous transmission — the span it covers is known to be partly empty. It is *not* the open cap, which is air **not yet measured**; this is air measured and found quiet.
- **`liveness`** — `live` (an interval intersecting the window is open at the live edge — i.e. its box runs to that edge), `ended` (its latest in-window interval is closed) or `absent` (no interval intersects; a Candidate in that state is simply not listed, a Confirmed catalogue entry is). **`ended_t_s`** is when it stopped, set exactly when `liveness` is `ended` and `null` otherwise — *"ended 4 minutes ago"*.
- **`silence_s` / `confidence` — the decayed rank (T-251, ADR-0017 TM-6).** `silence_s` is the time from the latest in-window interval's end to this window's live edge (`0` while on air, `null` when `absent`). `confidence` is the row's confidence in its own hypothesis, `0`–`1`:

  ```text
  confidence = 1                                          while the interval is open
  confidence = exp(−(silence_s − idle_gap) / idle_gap)     once it has closed
  confidence = 0                                          when liveness is absent
  ```

  **What it is for.** Window-scoping already removes a signal that stopped *hours* ago — it is not in the window. What it cannot rank is a signal that stopped **inside** the window, because `on_air_s` is blind to *when*: a row on air for the window's first five seconds and one transmitting right now for five seconds have identical `on_air_s`. `confidence` is the term that separates them, so a live list sorts by `on_air_s` **and** `confidence`, and a stopped row reads `ended` and ranks lower **without leaving the list** — its box is still on screen. **The time constant is the idle gap** (`clamp(2 × revisit_period, 1 s, 60 s)`, above), because the gap is one unit of *observed* absence: below it the receiver saw no absence at all, and past it it learns "still nothing" once per gap, so confidence falls `1/e` per observation. There is no free parameter and nothing to tune. **It is a rank, not a lifetime:** it expires nothing, deletes nothing and never touches History; it is re-derived from interval boundaries on every read, so a returning signal's new interval restores it to `1` with no revival step (and, per T-262, on the *same* row).
- **Nothing here reads `count`.** `count` is a lifetime History total and is excluded from every liveness decision and from live-list ranking (ADR-0017 §5) — it was the only column that could hold "this is still here", which is why it grew to 582 500/h. The open interval's advancing `t_end` is where that belongs. The interval's own `count` is deliberately not on the wire.
- **Scope.** With `t0`/`t1`, the window is exactly that range and `open` is derived against `t1` — a caller's `t1` *is* its own live edge — so scrubbing back re-derives the liveness a row had at that time rather than marking every past window `ended`. Without `t0`/`t1` the window is all of time up to now, against the wall clock: Explore's Confirmed list is deliberately not time-filtered and still has to render liveness. Interval closure uses the conservative 60 s idle gap (`hk_model::presence::IdleGap`), since the API does not know the scheduler's revisit period and a shorter gap would claim an absence nobody observed.
- **`at` — a live edge without a filter (T-263, ADR-0017 TM-7).** `t0`/`t1` do two things at once: they *select* which rows are listed, and they *scope* the projections above. A scrubbed-back Confirmed list needs only the second — windowing it would filter out the quiet catalogue entries §2.2 exists to keep listed — so `at` supplies the live edge alone: the window stays all of time up to `at`, every row that would have been listed is still listed, and `presence`/`liveness`/`ended_t_s` read as they did at that instant instead of now. It changes no predicate, so `total` and the listed rows are identical to the same query without it. `family_in_window` stays **absent** under `at`: naming a live edge is not asking about a window, and the two answers must not collapse. Given beside `t0`/`t1` it is refused (`400`) rather than ignored, since `t1` already is the caller's live edge.

  **Send it on every unwindowed query, not only while scrubbed (T-389).** Omitting `at` does not mean "no live edge"; it means the server supplies one, and the only one it has is `Timestamp::now()` — the **wall clock**. A client whose other surfaces run on the capture clock (a replay, the mock SDR, any source stamping from sample index) then reads its Confirmed rows' liveness on a second clock: measured on a fixture 3.6 days from wall time, the same three rows answer `liveness: "ended"`, `open: false` without `at` and `liveness: "live"`, `open: true` with it, so a transmitting station is listed as silent and its presence box loses its open edge. `at` selects nothing and costs nothing, so a live caller names its live edge exactly as a scrubbed one does.

**`family_in_window` (T-284, ADR-0017 §7.1).** The **same arbitration ladder** (`arb_rank` 0 user > 1 decoder > 2 lock-verified > 3 classifier > 4 track shape, latest among equals) run over **only the classification rows whose `t` falls inside `[t0, t1]`**. The ladder is unchanged and no ADR-0016 contract moves; only its *input set* gains a time predicate, so this is an additive projection.

- **`family` stays all-time and is never restricted.** Identity evidence is time-invariant — a CRC-valid decode from yesterday still says what the thing *is* — so `family`, `classification.family` and the `family` filter keep agreeing exactly as before.
- **`null` means the window holds no classification row.** Render `family` marked *(from earlier)*: the honest *"this is an FM station, but nothing in the last 30 s re-evidenced that"*, instead of silently asserting a stale classification. It never falls back to the all-time answer, and a fingerprint family carries no time so it never answers here.
- **The key is present only when `t0`/`t1` were given**, which is what keeps `null` ("a window was asked about and re-evidenced nothing") distinguishable from absent ("no window was asked about, so the question has no meaning"). A client sending no window renders `family` plain.

**`snr_db` / `peak_dbfs` (T-158).** The emitter's latest measurement: the peak SNR (`snr_peak_db`) and absolute peak level (`peak_level_dbfs`) of the newest (highest start time) detection linked to it, read directly off the stored `Detection` — no separate computation. "Linked" follows the same track a row's sighting created: a detection counted through one of the emitter's currently-linked tracks (the common case — sightings are almost always offered as tracks), or linked to the emitter directly. Both fields are `null` together when the emitter has no linked detection yet (e.g. an identity-only sighting from a decode, or a brand-new candidate before its track is offered). They are never derived from `recurrence` or any other summary field.

**`measured` (T-350) — the same measurement with the time it was measured over.** `snr_db` and `peak_dbfs` above are a `Detection`'s numbers, and a detection is a **time-frequency region, not a persistent carrier** ([ADR-0017](adr/0017-time-extent-signal-model.md)): its SNR is a fact about *that region*, over *that second or two*, not a standing property of the emitter. Served bare, the pair reads as current however old it is — and an inventory list, whose whole question is what is on the air in the viewed window, is exactly where a stale number looks live. `measured` is therefore the whole dated measurement in one object, so the levels cannot be read apart from their time:

- **`snr_db` / `peak_dbfs`** — the same two values as the flat fields, to the bit. They are repeated rather than moved so that no client breaks; the flat pair is the older spelling of the object, never a second measurement (the same relationship `cluster_id` has to `cluster_group`). A client that shows a level should read them from here, because reading them from here means it also holds the time.
- **`t_start_s` / `t_end_s` / `duration_s`** — the winning **detection's own** `TimeRange`, in seconds. Not the emitter's `first_seen`/`last_seen`, which are a *hull* and never an extent; not the request's window; not the time the answer was served. This is the same shape, for the same reason, as [`/api/analysis/strongest`](#get-apianalysisstrongest--strongest-signal-in-a-band-t-079)'s box carrying the extent of the **cell the peak was measured in** (T-337): reporting anything wider would be a guess dressed as a measurement.
- **`null` exactly when `snr_db` is.** All five values come from one detection, so there is no state in which a time exists without its levels or levels without their time, and a contract test asserts the equality and the pairing by value.

Detections remain **not** a served record kind (there is still no `/api/detections`), and this is why one is not needed for the level: the row carries the measurement and its extent together, rather than a reference a client would have to resolve to find out whether the number is two seconds or two days old.

**`relation` and `relations=` (T-219, C40).** One physical signal can produce several overlapping inventory rows, and a receiver can manufacture a row out of thin air. `relation` says why a row **defers to another**, or is `null` (the normal case): `{"kind", "artifact", "source_id", "author", "actor", "t_s", "reason", "score", "detail"}`, where `kind` is `suppressed-by` (the row overlaps a **Confirmed** entry's band by at least 60 % of **both** the narrower and the wider band, with nothing to tell the two apart — so a narrow emission sitting inside a wide one is never hidden by it), `duplicate-of` (the weaker of two overlapping candidates, ranked by a provisional SNR × duty × trust proxy — `score` — until decode evidence in bits exists), or `artifact-of` (a receiver artifact, with `artifact` ∈ `image` / `harmonic` / `intermod` and `detail` carrying the arithmetic: `n`, `a`, `b`, the tuning centre used, `predicted_hz`, `error_hz`, `tolerance_hz`, `suppression_db`, and (T-307) `receive_chain: {device_id, antenna_port}` — the front end the claim rests on, since T-302 gates every image/harmonic/intermod pairing on the chain and an artifact is a property of **one** receive chain, never a universal claim). `retune-sibling-of` (T-598: the **same LO-relative receiver artefact** as the row it names, seen from a different tuning centre, with `detail` carrying `slope` (`lo-locked` / `image`), `slope_factor`, `invariant_hz` — the coordinate `f − slope·f_LO` both sightings sit on — `centres`, `spread_hz` and `los_hz`; unlike every other kind the two bands do **not** overlap, because an LO-relative line lands at a different absolute frequency at each centre, which is exactly why one artefact was showing up as N emitters). `reason` is backend-rendered and never names an identity. Rows with a standing relation are **hidden by default** and listed with `relations=all`; the default is `relations=shown`, and an unknown value is `400 invalid`.

**Overlap is an error signal, and the resolution is re-analysis (T-369).** The three kinds above rank *hypotheses against each other*, and all of them are gated by that 60 %-of-both test — which is deliberately blind to the two geometries that actually stack boxes on a waterfall: a narrow box inside a wide one, and a staircase of offset boxes each overlapping the next by less than 60 %. Those pairs used to fall through every rule in silence and be served side by side. So after the ranking, any rows still overlapping in **time and frequency** are treated as proof the analysis is wrong (CLAUDE.md, "Overlap is an error signal that triggers re-analysis"), and their **region** — the connected component of overlapping boxes, closed transitively so the answer does not depend on which row a sighting happened to touch — is re-analysed against *the air* rather than against the rows: the measured `f_lo`/`f_hi` of every detection behind every member are merged into contiguous **modes**.

- **One mode** — the measurements never separated, so the boxes are cuts of one emission. The best-supported box is kept and the others become `duplicate-of` it, with `detail.verdict = "one-emission"` and `detail` carrying `region_lo_hz`/`region_hi_hz`, `mode_lo_hz`/`mode_hi_hz` and `members`. This is the one place a claim is made without the pairwise 60 % test — but **never** without the guard below, so the collapse can never merge two rows that anything tells apart.
- **More than one mode, or the guard blocks the merge** — nothing is merged and **nothing is hidden**: both rows stay listed with `relation: null`, and the finding is recorded as a revoked (`active = 0`) row carrying `detail.verdict = "contested"`, `detail.blocked_by` (the guard reason, e.g. `separated -3 dB extents`) and `detail.modes`. It is history, not a standing claim, so it is served by neither `relation` nor `relations=all` — read it from the emitter's relation history. Merging is the dangerous direction, so an overlap that cannot be resolved confidently is left alone and explained rather than guessed at.

**It terminates, by construction and by count.** The re-analysis does not recurse: it runs once per resolution, over a region bounded to 32 rows, and only appends. A claim is idempotent, so a region that resolves stays resolved with no further writes; a region that does not resolve appends at most `hk_model::relate::REGION_MAX_ROUNDS` (3) contested verdicts per row and then stops writing entirely.

**Nothing is ever deleted or overwritten by this.** A deferring row keeps its id, count, detections, tracks, links and history, is still reachable at `GET /api/inventory/{id}`, and its claim is append-only and reversible — later evidence revokes it and the row is listed again. A relationship is ranked evidence with its reasoning disclosed, never truth (the exploration-first rule). **The guard:** band overlap alone only makes two rows compete; any distinguishing evidence blocks a claim, in order — two different decoded identities, measured bandwidths further apart than the clustering ratio (checked on the measurement, so it holds for rows with no fingerprint), a fingerprint distance beyond tolerance (excluding the centre always, and excluding duty cycle, burst length and period when the two rows' presence intervals are disjoint — those measure the window each row was watched over, not the emission, so one station that stops and returns is not split in two; T-250), then −3 dB extents separated by more than the measurement uncertainty. Two genuinely distinct adjacent stations therefore stay two entries. An `artifact-of` claim needs more than arithmetic: the measured bandwidth must match the width the mechanism implies (an image preserves it, an `n`th harmonic scales it by `n`), the level must be 10–80 dB below the source, the row must have been seen only while the source was on air, and the detection must itself carry the matching suspect flag (`image_candidate`, `spur_candidate` or `suspect_imd`) — `detail.corroborating_flag` names it. Rules and thresholds: `hk_model::relate`; ADR-0015 §11.4.

**`classification` / `latest_classification` (T-211, ADR-0016 §2).** `classification` is the classification that sets `family`: the lowest **arbitration rank** (`arb_rank` 0 user > 1 decoder > 2 lock-verified > 3 classifier > 4 track shape), latest among equals, so `family`, `classification.family` and the `family` filter always agree. `latest_classification` is the most recently appended row when that is a different row (e.g. a later rank-3 `unknown` under a rank-2 lock-verified label), else `null`. Both have the same shape, or are `null` when the emitter has no classification:
- `family`, `confidence`, `open_set_score`, `model_version`, `t_s`: as before.
- `stage` (`feature-tree` / `verifier` / `dl` / `decoder` / `user` / `chain` / `track-shape`) and `arb_rank` (0–4). They are always set: a row written before M3 derives them. A `model_version` starting `decoder:` gives `decoder`/1, a track input gives `track-shape`/4, and anything else gives `chain`/3.
- `taxonomy` (e.g. `"hk-mod@1"`), `coarse` (`analog` / `digital` / `noise-like` / `unknown`), `class` (`{label, p, stage}` within the family, or `null` below its gate), `top` (≤ 5 posterior labels `{label, p}`, highest first, `unknown` included), `entropy_norm` (0–1) and `flags` (`prior-tiebreak`, `prior-mismatch`, `below-gate`, `suspect-input`, `dl-shadow-disagrees`). All are `null` on a row written before M3 or by a pre-M3 writer.

The full classification (likelihood, prior, provenance, reasons) is not on the row; it is served per emitter by `GET /api/inventory/{id}/classification` (T-247, below). `family` values on M3 rows are `hk-mod@1` families (`analog`, `fsk`, `psk-qam`, …, or `unknown`); pre-M3 rows keep their labels (`wfm`, `2fsk`, decoder and service ids).

**`cluster_id` (T-202, ADR-0016 §5).** The C18 cluster of unknown emissions this row currently belongs to — *"I have seen this before"* — or `null`. It is **evidence, never identity**: a cluster groups emitters that **measure** alike and sets nothing on the row (not identity, not family, not `known_status`, not lifecycle). Two identical sensors share a cluster and stay two inventory rows: a cluster is a *type*, an emitter is an *instance*. Only a *visible* cluster is named (at least three member emitters, or one emitter seen in at least three separated appearances) — below that the group is still a guess and reads `null`. The id is `null` on a withheld-identity row whatever storage holds, exactly as `estimated_params` and `/api/inventory/{id}/decode` are (T-159/T-163), so membership can never confirm a withheld identity indirectly. Details, members and history: `/api/clusters` below.

**`cluster_group` (T-320).** The same membership, served as **grouping data** so a list can show *which* rows measure alike instead of only that each one has been seen before. `null` exactly when `cluster_id` is (no cluster, not yet visible, or a withheld-identity row), else `{"cluster_id", "label", "rows_in_view"}`:

- **`label`** is a short, stable form of `cluster_id` (`hk_model::cluster_label`): the same cluster always reads the same label, two different clusters read differently, and it is derived from the id alone — so it cannot vary with which front end reported the row (T-259/T-305: dedup, clustering and identity never read the device).
- **`rows_in_view`** is how many rows **in this response** carry that id. It is scoped to what was served, never the cluster's total membership (`/api/clusters/{id}` answers that), so a client can say *"11 of the rows you are looking at measure alike"* and nothing stronger. On `GET /api/inventory/{id}` — one row, so one view — it is `1`.
- **It groups; it does not merge.** Rows sharing a label stay separate inventory rows with their own ids, counts, detections and history, exactly as `cluster_id` says: a cluster is a *type*, an emitter an *instance*, and clustering writes nothing on an emitter. Duplicate rows are minted upstream by entity resolution (`Fingerprint::compare`); this field makes such duplication **visible** and de-duplicates nothing. A client must render it as *"same signature cluster"*, never as *"duplicate"* — the former is what was measured, the latter a claim the data does not support. The collapse of genuinely overlapping rows is a different mechanism entirely (`relation`, `duplicate-of`, T-369).
- **It is not computed by the client.** Only the server knows what it served, so the grouping arrives as data (CLAUDE.md, "the web UI is a thin client over `docs/api.md`").

**`estimated_params` (T-163, ADR-0013 gap 7a).** The emitter's latest blind-estimated parameters (C13/C14) — symbol rate, modulation, deviation, carrier frequency offset, bandwidth — from its most recent demodulation session, for the Decode workbench's "Use" suggestions (`docs/adr/0013-ui-architecture.md` §4.6). `{"modulation", "symbol_rate_hz", "mod_order", "deviation_hz", "cfo_hz", "bandwidth_hz", "roll_off", "pilot_hz", "t_s", "source_session", "source_recording"}`. `modulation` is the session's `mode` (e.g. `wfm`, `2fsk`); every other measurement field is `null` when the estimator never measured it for this signal (e.g. `symbol_rate_hz` on an analog FM station) — **measured values only, never a fabricated default.** `t_s` is when the session ended; `source_session` is the demodulation's own id and `source_recording` the replayed recording's id, `null` live (matching `/api/inventory/{id}/decode`'s `at`/`source_session`). The whole object is `null` when no demodulation session has run for this emitter yet, and — like `/api/inventory/{id}/decode` (T-159/T-036) — also `null` on a withheld-identity row whatever storage holds, so the answer there is indistinguishable from "nothing measured yet" and never confirms a withheld identity indirectly. An emitter with no decoded identity at all (the common case for an unknown signal) is served normally.

**`total` (T-171).** The number of rows the query's filters match, ignoring `cursor`/`limit`, so a UI can show a count past one page (e.g. "512 confirmed" instead of capping at "500+"). It is computed with the same filters as the list, as a single indexed `COUNT(*)` — except a `tag` filter naming a label outside the controlled vocabulary (gating hides such tags on a withheld-identity row, so matching them needs per-row checks SQL alone can't do): that path scans and gates up to 5 000 candidate rows and reports the match count found within that scan, a lower bound past the cap. That combination (a non-vocabulary tag filter over a very large inventory) is rare.

**Lifecycle (T-078).** Every emitter starts `candidate`. An auto rule (e.g. a continuous trust-confirmed track, or a valid decode/identity) or a user promotes it to `confirmed`; a user (or nothing) can delete either. `deleted` is final for that row — it leaves the default list and entity resolution, but its detections, tracks, links and history are kept (visible with `state=deleted`); a later sighting of the same signal creates a *new* candidate. See `docs/07` §2.11.

### `/api/inventory/{id}` — one entry, promote, delete (T-078), user band (T-191)

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/api/inventory/{id}` | – | One entry (same row shape as a list entry above; deleted entries included) |
| POST | `/api/inventory/{id}/promote` | `{"reason"?}` | `{"changed", "entry"}` — candidate → confirmed; `changed: false` when already confirmed |
| DELETE | `/api/inventory/{id}` | `{"reason"?}` | `{"deleted": entry}` |
| PUT | `/api/inventory/{id}/band` | `{"f_lo", "f_hi", "reason"?}` | `{"user_band", "entry"}` — sets (replaces) the user band override |
| DELETE | `/api/inventory/{id}/band` | `{"reason"?}` | `{"cleared", "entry"}` — clears it; `cleared: false` when there was none |

`{id}` may be the id of an entity that has since been merged into another (the API resolves to the live emitter). `reason` (optional on the mutating routes) is a free-text string of up to `LIFECYCLE_TEXT_MAX` bytes; promote and delete are audited (`inventory_promote`, `inventory_delete`) with the old/new lifecycle state and the token fingerprint as actor. Errors: `404 not_found` (unknown id, or an entry already deleted), `400 invalid` (unknown body field, bad `reason`, a band breaking the rules below), `503 unavailable` (no inventory store or no audit log).

### `GET /api/inventory/{id}/classification` — the full classification (T-247, ADR-0016 §2/§9)

| Method | Path | Query | Response |
|---|---|---|---|
| GET | `/api/inventory/{id}/classification` | – | `{"emitter", "classification": Classification \| null, "latest": Classification \| null}` |

What the C15 classifier measured about one emitter, in full. The inventory row carries only the summary (`family`, `confidence`, `top`, `coarse`, `class`, `entropy_norm`, `flags`); the parts too large for a list row are served here: both distributions (`posterior` and the prior-free `likelihood`, each over `hk-mod@1` families **including `unknown`**), the `prior` that was fused if any, `open_set_score`, `provenance` (`rules`, `features_version`, `snr_db` against its `snr_gate_db`, `gated`, `thresholds`, suspect flags) and the machine `reasons` (`low_snr`, `too_short`, `no_symbol_estimate`, …). **Every classification also states what the post-sync verifier did** (T-589): `verifier_confirmed` / `verifier_reranked` when it ran, or one of `verifier_abstained_upstream`, `verifier_no_class_call`, `verifier_single_candidate`, `verifier_no_clock_lock`, `verifier_no_model`, `verifier_geometry`, `verifier_no_symbol_view` when it did not. A stage that silently does not run reads identically to one that ran and agreed, which is how C14 failing to lock on every genuine 8-PSK snippet stayed invisible; the reason is always present so the two can be told apart and counted. The object is `hk_model::classify::Classification` exactly as ADR-0016 §2 defines it, served verbatim — so its own time field is `t_ns`, Unix nanoseconds (the inventory row's summary restates the same instant as `t_s` in seconds). Code: `crates/hk-api/src/classification.rs`; written by `hk_pipeline::classify`.

- **`classification`** is the row that sets the emitter's `family` — the arbitration winner (lowest `arb_rank`, latest among equals) — or `null` when that row was written before M3 or by a pre-M3 writer, which carries a bare family string and no distribution.
- **`latest`** is the most recently appended row when that is a *different* row, else `null` — the same rule as the row's `latest_classification`. **This is the normal shape for a demodulated emitter:** ADR-0016 §2 leaves the family to the demodulator chain that locked (rank 2), while the C15 cascade's posterior is recorded at rank 3 beside it, so a client that reads only `classification` would report `null` for an emitter the classifier did measure. Read `latest` first and fall back to `classification`.
- Both are `null` for an emitter nothing has classified yet. That is not an error, and it is not the same as a classification of `unknown` — `unknown` is a measured open-set outcome with a posterior behind it, while `null` means nothing has measured this emitter at all.
- **`provenance.features_version` (T-290).** The version of the C15 feature vector the classifier measured with (`features@N`, ADR-0016 §4.2) — not the C18 `EmissionFeatures` field set, which versions separately. **`1` means indeterminate, not "version 1":** every row written before T-290 carries `1` whatever vector produced it, because the classifier restated its provenance constant as `1` while the vector moved to `2` and then `3`, and those versions differ in what the `symmetry` dimension measures and whether it is measured at all. A client must not read `1` as a version, order or compare it against a current one, or infer that such a row is old. Rows written since T-290 name the vector exactly, so **`features_version > 1` is the test for "this row's feature set is known"**. No stored row is rewritten: `emitter_classification` is append-only, and which of the three vectors a `1` row used is not recoverable.
- **Evidence, never identity.** A classification is a distribution with an explicit `unknown` outcome and its reasoning disclosed — ranked evidence beside the measurement, exactly like a band-plan prior or a signature match. Only a CRC-valid decode confirms what a signal is (ADR-0016 §4.7), and a high `confidence` is never a licence to present the family as fact. A client must present it as a suggestion.
- **Resolution.** Like `/api/inventory/{id}`: a merged id resolves to its live survivor; an unknown or unparsable id is `404 not_found`.
- **Gating.** Like `/api/inventory/{id}/decode` and `/api/signatures/match`, an emitter whose decoded identity is withheld answers exactly as one nothing has classified — no flag, no count, no marker — so this route can never confirm a withheld identity indirectly. An emitter with no decoded identity is served normally.
- **Errors** `{"error", "code"}`: `404 not_found` (unknown or unparsable id), `503 unavailable` (this server has no inventory), `405` other methods.

### `GET /api/inventory/{id}/decode[?t0=&t1=]` — decode fields for a window (T-159; window T-384, ADR-0013 §3.3.1)

The emitter's decoded fields: the focus panel's "Decoded summary" and per-signal output panels (RDS PS/RT for FM, decoded records for digital recipes; docs/14 "Added scope from docs/15 §7"). `{id}` resolves like `/api/inventory/{id}` (a merged id resolves to its live survivor). No parsing happens in the UI — every field here is as the decoder or recipe committed it.

```jsonc
{
  "decodes": [
    { "decoder": "hk-rds", "recipe_id": null, "frame_model": "rds-pi", "at": 1789300820.5,
      "fields": { "pi": "C0DE", "pty": 10, "tp": false, "ta": false, "ps": "KROQ    ",
                  "pi_votes": 12, "pi_total_votes": 12, "pi_share": 1.0,
                  "groups_ok": 41, "groups_total": 42, "block_error_rate": 0.01 },
      "crc": { "valid": true },
      "source_session": "0199…" }
  ]
}
```

**The window (`t0`/`t1`, T-384).** Given together, Unix **seconds** on the capture clock, closed on both ends: the answer is *what was decoded in that window* — one row per `(decoder, frame_model)` that produced anything inside it, each row the latest **within the window**. Without them it is the latest of all time, exactly as before. A half-given pair (`t0` with no `t1`, or the reverse) and `t1 < t0` are `400 invalid`, as is a nanosecond value in the seconds parameter (both ends are bounded to |t| < 9×10⁹ s), so an invented or misread window can never succeed and return a plausible-looking zero rows.

This exists because the output/decode panels are views over the UI's one (time × frequency) window like every other surface (ADR-0013 §3.3.1), and this route had **no time parameter at all** — a panel scrubbed back an hour could only keep rendering the live edge's fields and label them the window's. Two rules follow, and both are asserted by value:

- **It filters; it never re-decodes.** A `Decode` row is what the decoder or recipe already committed, carrying its own capture-clock `at`. Serving a past window selects stored rows, so CLAUDE.md's incremental-decode invariant (*live decoding extends the region's time extent and decodes only the newly-arrived part, never re-decoding what is already done*) holds trivially: no decoder runs here, and re-running one to answer a scrub would break it.
- **Latest-per-frame-model is computed after the filter, not before.** The other order answers "the all-time latest row, if it happens to fall in the window", which reports *nothing* for a window that plainly holds an older row — data that exists and would not be rendered.

**Not `/api/captures/{id}/frames`.** That route is keyed by *capture* (one recording of one pipeline's decoded stream), already carries `from_t`/`to_t`, and serves raw stream records at frame granularity — the right route for the packet inspector's own scrubbing. It cannot answer *what has this emitter decoded*: there is no emitter→capture key to follow (the `decode` table has neither an emitter nor a capture column; it is keyed by decoded identity), and assembling an RDS station name out of raw group records in the client would be both a thin-client violation and the re-decoding the invariant above forbids. The two stay distinct: this route is emitter-keyed and serves committed fields; that one is capture-keyed and serves records.

One row per `(decoder, frame_model)` pair the emitter's decoded identity has produced, newest first: a plugin decoder (`readsb`, or the built-in `hk-rds`) commits one frame model per row, while a recipe's several `messages` outputs share one `decoder` (`recipe:<id>`, e.g. `rds.recipe.json`'s `group-info`/`station`/`radiotext` outputs) but each names its own `frame_model`, so RDS's PI/group metadata, PS and RadioText each get their own row. `recipe_id` is the id after `recipe:` when `decoder` has that prefix, else `null`. `fields` merges the row's metadata (frame type, addresses, counts — always stored) and content (payload/text, only when the caller's identity access reveals it; content gating is off by default, T-143) into one object, exactly as the decoder/recipe committed them. `crc.valid` is whether the frame's check passed; `source_session` is the producing `Demodulation`'s id, else the replayed `Recording`'s id, else `null` for a live decode with neither. An emitter with no decoded identity, or one content gating withholds, answers `{"decodes": []}` — the same lookup never confirms a withheld identity by naming its decodes (T-036). `404 not_found` for an unknown id.

**Evidence rule (T-185/T-210).** A CRC-invalid frame never reaches a row: the `fields` block's `skip_invalid` drops it before parsing, so `crc.valid` is `true` for every row today. T-210 (in progress) adds bounded RDS block error correction with consensus-gated PI/PS/RT commits; **TODO(T-210):** once corrected-group provenance lands on `Decode`, add `crc.corrected` here without changing what `crc.valid` means.

**User band (T-191).** Every row (list and one entry) carries `user_band`: `null`, or `{"f_lo", "f_hi", "set_at", "actor", "reason", "reason_withheld"}` — edges in Hz, `set_at` in Unix s, `actor` the token fingerprint, `reason` the user's note or `null` (withheld, with `reason_withheld: true`, on a withheld-identity row like any user-authored reason). It is a user's adjustment of the band edges (e.g. dragging a confirmed signal's box) stored **beside** the measured band: `f_center_hz`/`bandwidth_hz`/`f_lo_hz`/`f_hi_hz` stay what blind detection measured and are never overwritten. Rules for `PUT`: `f_lo` and `f_hi` finite numbers with `0 < f_lo < f_hi`; width `f_hi − f_lo` ≤ 40 MHz (`hk_model::USER_BAND_MAX_WIDTH_HZ`); and the band must overlap the measured `[f_lo_hz, f_hi_hz]` or lie within 1 MHz of it (`USER_BAND_MAX_GAP_HZ`; both limits inclusive) — an adjusted edge, not a different signal. Set and clear are both audited as `inventory_band` with `old`/`new` = `{"id", "user_band"}` and the token fingerprint as actor. The override survives restart; when two entries merge (same emission, T-082) the survivor keeps an override if either had one, the latest `set_at` winning (a tie keeps the survivor's). The pipeline never uses it for detection, tracking or entity resolution; consumers that tune to an entry (Listen, Decode, recipes' `{emitter_id}` target) still use the measured band today and may prefer `user_band` later.

### `GET /api/events` — the durable catalogue of events (T-264, ADR-0017 TM-8)

**The History surface's route** (product vision workflow #3: *choose a region and see what activity was seen there over time*). `/api/inventory` answers *"what emitters do I know here"* and Explore scopes it to the viewed window (T-260); this answers *"what has happened here"*, over all of the recorded past, and **the unit is the event, not the emitter**: one row per presence interval, so a one-off burst is a first-class row with its own timespan rather than a blip that never qualified as a live candidate (CLAUDE.md invariant 1). Code: `crates/hk-api/src/events.rs`.

`f_lo`, `f_hi`, `t0` and `t1` are **required** — History is a question about a box. Every other `/api/inventory` filter is accepted with exactly the same meaning and selects which *emitters* are expanded (so the two surfaces can never disagree about which rows a box holds); `limit`/`cursor` page the **events**.

```jsonc
{
  "window": { "f_lo_hz": 101.2e6, "f_hi_hz": 101.4e6, "t0_s": 1789214400.0, "t1_s": 1789300800.0 },
  "events": [
    { "emitter_id": "0199…", "t_start_s": 1789300871.0, "t_end_s": 1789300871.04,
      "duration_s": 0.04, "in_window_s": 0.04, "open": false, "count": 1, "sources": 1,
      "f_center_hz": 915200000.0 }
  ],
  "emitters": [
    { "id": "0199…", "state": "candidate", "f_center_hz": 915200000.0, "bandwidth_hz": 120000.0,
      "f_lo_hz": 915140000.0, "f_hi_hz": 915260000.0, "known_status": "unknown", "family": null,
      "explanations": [ { "rank": 1, "service": "band-plan", "label": "ISM 902–928 MHz", "score": 0.4, "flags": [] } ],
      "identity_scheme": null, "identity_class": null, "withheld": false,
      "events": 1, "on_air_s": 0.04, "liveness": "ended", "count": 1 }
  ],
  "total": 1, "limit": 200, "next_cursor": null,
  "emitters_truncated": false, "emitters_no_interval": 0,
  "coverage": { "source": "spectrum-history", "observed_fraction": 0.97, "cells": 76800,
                "observed_cells": 74112, "gaps": [ { "t0_s": 1789260000.0, "t1_s": 1789260900.0 } ],
                "gaps_truncated": false,
                "statement": "97 % of this region was observed; 1 unobserved stretch in this period is no data, never a quiet band." },
  "identity_access": "standard"
}
```

- **An event is a presence interval** (`hk_model::presence`, docs/07 §2.27), derived from the append-only `emitter_observation` ledger by the same `Repository::presence_intervals` the inventory row's `presence` uses — and, since **T-591**, through the same *call*: `ObservedCoverage::track` is the one derivation of a presence track in `hk-api`, and this route, `/api/inventory`, `/api/inventory/{id}/presence` and `/api/tiles/events` all reach it there. It used to hard-code `IdleGap::conservative()` (60 s) while `/api/inventory` measured the gap off the band's tune history (T-410), and the two surfaces disagreed about one emitter's liveness in the same window — 3 of 3 events `open: true` beside rows reading `ended`, measured on T-254's ISM burst scene. The idle gap is **measured**, and one derivation is what makes that promise structural rather than two constants that happen to match. `duration_s` is the event's own length and `in_window_s` its intersection with the request — **both computed here**, because a client must never derive a timespan from two fields it was handed. `open` is read against the window's own `t1` (a caller's `t1` *is* its live edge, exactly as on `/api/inventory`), so a past window re-derives the truth of its own moment instead of being marked ended by the wall clock. `count` on an event is the sightings its source rows summed — a History total, and never a liveness or ranking input (ADR-0017 §5).
- **`emitters[]`** lists each emitter with at least one event in the window, once, with its ranked `explanations` — suggestions beside the measurement, never truth (vision step 4). Its `events` and `on_air_s` describe the whole window, not the current page; `count` is the lifetime total, which is exactly where a monotonic counter belongs. Identities are gated as on `/api/inventory` (`identity_value` only when in clear).
- **`liveness` (T-591)** — `live` / `ended` / `absent` for this emitter over **this** window: byte-for-byte the `presence.liveness` `/api/inventory` serves for the same emitter and the same `t0`/`t1`, because it is the same projection of the same track under the same measured gap (`PresenceTrack::project`). Liveness is a property of the emitter — one interval `[start, end?]`, ongoing until an end is affirmatively detected and revocable afterwards (ADR-0017/0019) — never a property of the route asked, so **the two surfaces cannot disagree**; an acceptance test asserts equality over *every* emitter in the window and reports how many it compared. Serving it here also removes the reason a client had to reconstruct liveness from the `open` flags itself.
- **Nothing here can be erased by decay** (ADR-0017 conflict (b)). What decays (T-251/TM-6) is a *candidate's confidence* — a ranking over hypotheses that writes nothing and deletes nothing; this route never reads it. A signal that stopped hours ago left Explore because it is not in the window, and its events are still catalogued here. A **deleted** emitter's events are still recorded too, and are listed with `state=deleted` like any other inventory read.
- **`coverage` keeps three answers apart, and never collapses them.** `statement` is backend-rendered and is what a client shows beside an empty catalogue: with no spectrum history on the server, coverage is **unknown** and an empty answer is evidence of nothing; with `observed_cells: 0` it reads *no data for this period, not a quiet band*; with `gaps[]` it says how much was observed and that the gaps are no data; only a box observed throughout says an empty catalogue means nothing was on the air. Unobserved is never reported as quiet (C26), and `gaps` are the fully unobserved time runs of `/api/history`'s own grid.
- **Bounds, disclosed rather than silent.** At most 500 emitters are expanded per answer (`emitters_truncated` when more matched the box); `total` is the events found over those emitters, and `limit`/`cursor` page them newest first. `emitters_no_interval` counts rows the box selected that carry **no presence interval at all** (a legacy writer's row, matched on its `first_seen`/`last_seen` hull): they contribute no event, because a hull is not a timespan and inventing one would fabricate a measurement — so the count is disclosed instead.

### `GET /api/inventory/{id}/presence` — one emitter's presence track (T-264, ADR-0017 TM-8)

The row's `presence` object is a *projection through a window* (how many intervals intersect it, time on air inside it, the latest one, liveness). This is the **track itself** — every interval with its own timespan — which History needs for one row and a live list must never carry.

```jsonc
{
  "emitter": "0199…",
  "window": { "t0_s": 1789214400.0, "t1_s": 1789300800.0 },   // null when none was asked for
  "intervals": [ { "t_start_s": 1789300871.0, "t_end_s": 1789300920.0, "duration_s": 49.0,
                   "open": true, "count": 12, "sources": 2, "f_center_hz": 101300000.0 } ],
  "total": 1, "truncated": false,
  "presence": { "intervals": 1, "on_air_s": 49.0,
                "last_interval": { "t_start_s": 1789300871.0, "t_end_s": 1789300920.0, "open": true, "revoked_s": 0.0 },
                "liveness": "live", "ended_t_s": null, "silence_s": 0.0, "confidence": 1.0 }
}
```

`t0`/`t1` (given together) scope the track and supply its live edge; without them it is all of time up to now. `intervals` is newest first, at most 5 000 (`truncated`, `total` unbounded by the cap — the cap keeps the newest, since the older end is reachable by asking about an earlier window). `presence` is the same projection `/api/inventory` serves on the row — rendered by the same code, so the two surfaces cannot disagree about one emitter's liveness or its decayed `confidence` (T-251), and a contract test asserts the two answers are equal field for field. `duration_s` is computed here for the same reason as on `/api/events`. `{id}` resolves like `/api/inventory/{id}` (a merged id resolves to its live survivor; unknown or unparsable is `404 not_found`). Unlike `/decode` and `/classification` the answer is **not identity-gated**: timing is data of exactly the class `presence` and `recurrence` already carry unconditionally on every row (T-284), and serving it uniformly is what stops its presence from signalling that a row's identity was withheld.

### `GET /api/analysis/strongest` — strongest signal in a band (T-079)

Backend replacement for client-side peak-picking over a locally held spectrum row (see [UI decision logic moved server-side](#ui-decision-logic-moved-server-side-t-079) below): the strongest observed signal (max-hold, dB/Hz) in `[f_lo, f_hi)` over the last `window_s` seconds, read from the same spectrum-history pyramid as `/api/history`. The window ends at the stream time the history has reached (the end of its newest frame), not the wall clock, so a replay or a time-compressed scene (T-125) is queried on its own clock; before any frame it ends at the wall clock.

```jsonc
{ "found": true, "f_center_hz": 101300000.0, "f_lo_hz": 101200000.0, "f_hi_hz": 101400000.0, "max_db": -71.2,
  "t_start_s": 1789300812.0, "t_end_s": 1789300813.0, "duration_s": 1.0, "t_cell_s": 1.0,
  "window": { "t0_s": 1789300810.0, "t1_s": 1789300815.0 },
  "semantics": { "statistic": "max-hold", "scale": "dbfs-per-hz", "rule": "max-hold: … the max of nothing is unobserved, not zero" } }
```

or `{"found": false, "window": {…}}` when nothing was observed in the window. `semantics` (T-342) states on the wire what the docs above say in prose — that `max_db` is a **max-hold**, and the scale it is in (`"dbfs-per-hz"` uncalibrated, `"dbm-per-hz"` at the antenna port) — so a consumer never has to infer either. It is the same statement the band-collapsed series on [`/api/timeline`](#the-band-collapsed-series-and-why-the-response-states-its-own-semantics-t-342) carries, which is what makes the two routes siblings rather than two different ideas of "strongest". Unlike a live FFT row, spectrum-history cells carry no per-bin skirt to fit a box to, so the reported box is a fixed **±100 kHz** around the strongest cell's centre, clamped to `[f_lo, f_hi)` — not a measured signal bandwidth. `400` when `f_hi <= f_lo`, `f_lo`/`f_hi` are out of range, or `window_s` is not a finite number in `(0, 300]`.

**Time on the answer (T-337).** This route hands the UI a *box*, and a box has a time as much as a frequency ([One shared time axis](#one-shared-time-axis-t-337) below), so it carries one:

- **`window`** — `t0_s`/`t1_s`, the window actually searched. Present on `found: false` too, so "nothing in the last 5 s" and "nothing in the last 300 s" are different answers. A client never reconstructs it from its own `window_s` and the moment the reply arrived: the window ends at the stream time the history has reached, which on a replay or a time-compressed scene is not the wall clock at all.
- **`t_start_s` / `t_end_s` / `duration_s`** (found only) — the box's own time extent: the time extent of the **pyramid cell the peak was measured in**, not the whole window. `t_cell_s` repeats that cell size so the block is self-contained. Reporting the window as the box's extent would be a guess dressed as a measurement; reporting the cell says exactly when the strongest thing was strongest, to the resolution the history holds.

### `GET /api/navigation` — the achievable `(centre, span)` grid (T-341)

**The rule, from the user** (CLAUDE.md, "Time, the waterfall, and the live view", invariant 6):

> Navigation is discretized to achievable capture states, and the UI never implies detail the front end can't deliver. Zoom/pan and region-select resolve only to **realizable** configurations and **snap to the nearest one**: in frequency, centre and span are bounded by the instantaneous bandwidth (sample rate) and the tuning step — wider than the live window is **survey-history overview**, not live IQ; in time, by the retained window bounds and the history pyramid's discrete resolution tiers. The view must distinguish **live-IQ-backed detail** from **survey-/spectrum-history overview**, so a wide or deep zoom never fakes resolution the hardware did not capture.

This is **absent-means-not-measured** (T-297: no field is written for a region that was never swept, so nothing ever writes a zero rate) applied to the navigation surface: **an interpolated pixel that looks like a measurement is a lie with a picture attached.**

**The split.** The backend reports the grid and owns which states are realizable; the client does the gesture, the snap arithmetic against the grid it was handed, and the styling. So this route is data, never a rendered axis — and it also answers the one question a client must not decide for itself: *for this requested state, which tier answers, and is it live IQ or overview?*

```jsonc
{
  "frequency": {
    "device_id": "hackrf:0000…f3c7", "driver": "hackrf-one", "controllable": true,
    "ranges_hz": [[1000000.0, 6000000000.0]],
    "center_step": "uniform", "center_step_hz": 28.6102294921875,
    "spans_hz": { "min": 2000000.0, "max": 20000000.0 },
    "max_live_span_hz": 20000000.0,
    "current": { "center_hz": 100000000.0, "span_hz": 2400000.0 }
  },
  "time": {
    "tiers": [ { "level": 0, "t_cell_s": 1.0, "f_cell_hz": 6250.0, "max_age_s": 3600.0 },
               { "level": 1, "t_cell_s": 60.0, "f_cell_hz": 12500.0, "max_age_s": null } ],
    "min_t_cell_s": 1.0, "max_t_cell_s": 604800.0,
    "latest_s": 1789300920.0
  },
  // T-340: every currently-active capture window, as a list. `[]` on a replay.
  "windows": [
    { "device_id": "hackrf:0000…f3c7", "driver": "hackrf-one",
      "center_hz": 100000000.0, "span_hz": 2400000.0,
      "f_lo_hz": 98800000.0, "f_hi_hz": 101200000.0 }
  ],
  // present only when center_hz and span_hz were given
  "resolved": {
    "requested": { "center_hz": 100000001.0, "span_hz": 40000000.0, "t_cell_s": null },
    "center_hz": 99999995.02..., "span_hz": 20000000.0, "t_cell_s": null, "level": null,
    "source": "survey-overview", "live": false,
    "statement": "survey overview: wider than one capture window, stitched from separate dwells, not live IQ",
    "matched": false, "snapped": ["center_hz", "span_hz"]
  }
}
```

**`frequency`** — the achievable `(centre, span)` grid of the live front end, or `null` on a run with no live device (a replay): there is no grid to report, and an invented one would be worse than none.

- `ranges_hz` — the centre bounds, as on `/api/control/state`.
- `spans_hz` — the span axis. **A live window's span *is* its sample rate**, so this is the rate capability: `{min, max}` for a continuous range, `{values}` for a discrete list.
- `center_step` / `center_step_hz` — **the axis added by T-341.** Three-valued like the bias tee: `"uniform"` with a step in Hz, or `"unknown"` with `center_step_hz: null` when the source cannot say. A client must never read `"unknown"` as 1 Hz or as continuous — an unknown grid has no nearest point, so **nothing snaps**, and `resolved.center_hz` comes back `null`. HackRF One reports `30 MHz / 2^20` = 28.6102294921875 Hz, the MAX2837 fractional-N granularity; a SigMF replay reports `"unknown"`, because a recording holds the centre it was made at and never the synthesiser grid of the device that made it.
- `max_live_span_hz` — the widest span that is still **one** capture window. Wider is survey overview by definition, whatever the pyramid can draw there.
- `current` — the tuned state, so a client can mark where it is on the grid.

**`time`** — the retained window and the pyramid's **discrete** resolution tiers, or `null` with no spectrum history on this server. Time resolution is a ladder, not a slider: a view asking for a finer cell than `min_t_cell_s` cannot be served one. `max_age_s` is `null` when a level sets no age of its own — *no age limit*, not *kept forever* (the byte budget still bounds it). `latest_s` is the newest capture time the **history** has reached; the capture-ring window that sizes the scrubber is a different horizon and a different length (`GET /api/timeline`, over `GET /api/iqbuffer`).

**`windows`** — **the currently-active capture windows, as a list** (T-340). Each entry is one live front end: its `device_id` (T-343's provenance identity, `null` when the source reports none — never a placeholder), `driver`, the tuned `center_hz`, the `span_hz` it is running at (a live window's span *is* its sample rate) and the window's edges `f_lo_hz`/`f_hi_hz`. The frequency navigator draws one **lit segment** per entry, on the whole device-available spectrum (`frequency.ranges_hz`).

*Why a list on a server that runs one front end.* The count is a fact about the run, not a constant of the design: the source layer is already N-shaped (T-259's audit; T-302/T-303/T-304/T-305 keyed artifacts, baselines, history and the source-layer rule on the front end that produced each frame), and multiple simultaneous windows are an explicit product direction. The array's length is therefore **measured, never assumed** — `[]` on a replay, one entry on a live run — and a client must place segments from the list rather than from `frequency.current`, which is one device's tuned state and not an enumeration. **T-511 made that literal:** `ApiState::live_controls` is now a collection keyed by `device_id`, built one handle per front end where the pipeline composes the run — and, exactly as this paragraph predicted, *neither this route's shape nor its clients changed*, because both already spoke in lists. Which radio a device route moves is now a [selector](#which-radio-the-device-selector-t-511) on those routes, not an assumption here.

**`resolved`** — present only when `center_hz` and `span_hz` are given together. The nearest realizable state, and the detail claim that comes with it.

- `center_hz` / `span_hz` / `t_cell_s` — the snapped values, each `null` when that axis cannot be snapped (an unknown tuning step, no rate reported, no history). A `null` is never the request echoed back: echoing it would claim the device can sit exactly there.
- `matched` / `snapped` — T-334's vocabulary, because it is the same question asked of a different grid: `snapped` names each axis that moved, and `matched` is `true` when none did.
- `level` — the pyramid level that would answer `t_cell_s`, when one was asked for.
- `source` / `live` / `statement` — the detail claim, below.

#### Live-IQ detail versus overview (T-341)

`resolution.source` on `/api/history` and `resolved.source` here are the same enum. T-334 shipped it as the constant `"spectrum-history"`, documented as the home for "which tier answered"; T-341 gives it its other two values. They are ordered by **how much detail they claim**:

| `source` | Meaning |
|---|---|
| `"live-iq"` | Live IQ from the front end, at the resolution drawn. The span fits inside one capture window. |
| `"spectrum-history"` | The tiered pyramid: measured, but reduced to a tier's cells rather than live IQ. Never interpolated. |
| `"survey-overview"` | Wider than any single capture window: no live window covered this span whole, so the picture is stitched from separate dwells. |

**Nothing may claim more than it can show.** The test is `span_hz <= max_live_span_hz`, with the boundary counted as *inside* — a view exactly as wide as the sample rate is one window's worth. One hertz beyond it is called overview, not nearly-live, because every extra hertz had to come from a different dwell. When no front end reports a window at all, the answer is `"survey-overview"`: not knowing the window is not evidence that the span fits inside it. `/api/history` never answers `"live-iq"`, because it reads the pyramid and only the pyramid; asking this route for a `t_cell_s` is likewise asking the pyramid, so it answers `"spectrum-history"` even for a span that would otherwise be live.

**Error direction on each axis, and why it is that way:**

| Axis | Errs | Because |
|---|---|---|
| centre | to a **coarser** grid than the hardware's, never finer | A coarser step offers fewer centres, all reachable. A finer one offers centres that do not exist, and the radio lands elsewhere while the axis claims otherwise. (HackRF: `hackrf_set_freq` accepts integer hertz, but 28 of every 29 such commands land on the same synthesiser point — a declared 1 Hz step would put 28 imaginary centres on the axis.) |
| span | **down** to an achievable rate | A span is clamped into the rate capability, so a view is never told it can have a window the device cannot open. |
| time cell | **coarser**, never finer (T-334's rule) | A coarse cell repeated across pixels shows a measured value; a fine grid reduced in the client invents one. |
| the claim itself | to the **weaker** claim | `live-iq` > `spectrum-history` > `survey-overview`. A surface that cannot establish the stronger claim makes the weaker one. Under-claiming costs a styling cue; over-claiming is the lie the invariant forbids. |

### `GET /api/timeline` — the capture window, and the overview drawn on it (T-338)

Query parameters (all optional): `f_lo`&`f_hi` (Hz, given together — the band to draw), `columns` (1…4096, default 96), `rows` (1…512, default 1). **There is no `t0`/`t1`:** the time extent is not the caller's to give, and that is the whole point of the route.

The asymmetry is deliberate and it is the route's shape: **the window is the server's, the band is the caller's.** `window` is the same span whatever `f_lo`/`f_hi` say, and `grid` is that window folded over the band asked for. So the two callers that draw a time axis — the capture band and the **time navigator** (T-367) — each pass the frequency range *their* view is on, and get the retained capture for that range rather than for the whole spectrum. With no band there is no `region` and no `grid`: a picture of "everything" is a different measurement, not a default (see the control in `the_timeline_spans_the_capture_window_and_draws_it`).

```jsonc
{
  "window": { "horizon": "iq-ring", "enabled": true, "reason": null,
              "retention_s": 120.0,
              "t0_s": 1789300800.0, "t1_s": 1789300920.0, "span_s": 120.0,
              "buffered": { "t0_s": 1789300890.0, "t1_s": 1789300920.0, "span_s": 30.0 } },
  "region": { "lo_hz": 99600000.0, "hi_hz": 102000000.0 },
  "grid": { "nt": 96, "nf": 4, "t0_s": 1789300800.0, "t_cell_s": 1.25,
            "f_lo_hz": 99600000.0, "f_cell_hz": 600000.0,
            "max_db": [ -102.4, null, "…" ], "occupancy_max": [ 0.5, null, "…" ],
            "coverage": [ 1.0, 0.0, "…" ], "frames": [ 25, 0, "…" ],
            "cells": 384, "observed_cells": 96,
            "range_db": { "lo": -138.2, "hi": -91.0 }, "unit": "dbfs",
            "semantics": { "fold": "max-hold", "rule": "max-hold: a cell is the maximum of the source cells folded into it, … the max of nothing is unobserved, not zero",
                           "unobserved_rule": "… null is never observed, never quiet, and is not the bottom of the scale",
                           "series": { "max_db": { "statistic": "max-hold", "scale": "dbfs-per-hz", "unobserved": "null" },
                                       "occupancy_max": { "statistic": "max", "scale": "fraction", "unobserved": "null" },
                                       "coverage": { "statistic": "extent-weighted-mean", "scale": "fraction", "unobserved": "0" },
                                       "frames": { "statistic": "sum", "scale": "count", "unobserved": "0" } },
                           "range_db": "the observed minimum and maximum of `max_db` over this grid, …" } },
  "coverage": {                                   // T-423: record-derived, per drawn cell
    "grid": { "nt": 96, "nf": 4, "t0_s": 1789300800.0, "t_cell_s": 1.25,
              "f_lo_hz": 99600000.0, "f_cell_hz": 600000.0,
              "aligned": true, "order": "row-major: cells[t * nf + f], … the same layout as `grid`" },
    "devices": [ { "device": "hackrf:0000…925f", "named": true,
                   "observed_cells": 30, "unobserved_cells": 66, "unknown_cells": 0,
                   "observed_fraction": 0.31,
                   "cells": [ { "state": "unobserved" },
                              { "state": "observed", "spans": 1, "observed_s": 1.25, "duty": 1.0,
                                "last_s": 1789300920.0, "center_hz": 100800000.0,
                                "sample_rate_hz": 2400000.0 }, "…" ] } ],
    "any": { "device": "any", "named": false, "…": "…" },
    "horizon": { "oldest_record_s": 1789300890.0, "recording_began_s": 1789300802.5,
                 "forgotten": null, "as_of_s": 1789300920.0,
                 "unknown_from_row": 2, "unknown_rows": 70, "rows": 96,
                 "rule": "a row wholly before `oldest_record_s` … is \"unknown\" - UNLESS … wholly before `recording_began_s` …",
                 "state_rule": "\"unknown\" carries no measurement keys, exactly like \"unobserved\" …" },
    "sources": [ { "kind": "iq-ring", "spans": 37, "named_spans": 37, "device_known": true, "available": true },
                 { "kind": "observation-log", "spans": 12, "named_spans": 12, "device_known": true, "available": true },
                 { "kind": "open-dwell", "spans": 1, "named_spans": 1, "device_known": true, "available": true } ],
    "rule": "record-derived: whether the front end was TUNED to this cell … `grid.coverage` is a different measurement …"
  },
  "resolution": { "source": "spectrum-history", "live": false, "statement": "…",
                  "horizon": "iq-ring",
                  "served_span_hz": 2400000.0, "max_live_span_hz": 20000000.0,
                  "level": 0, "levels": 5,
                  "t_cell_s": 1.25, "f_cell_hz": 600000.0,
                  "src_t_cell_s": 1.0, "src_f_cell_hz": 6250.0,
                  "requested": { "columns": 96, "rows": 4 },
                  "served": { "nt": 96, "nf": 4, "cells": 384 },
                  "reduced_from": { "nt": 120, "nf": 384 },
                  "budget": { "time": { "requested": 96, "served": 96, "source_cells": 120, "replicated": false },
                              "frequency": { "requested": 4, "served": 4, "source_cells": 384, "replicated": false },
                              "statement": "the served grid is exactly the budget asked for: … the window is never truncated to fit it. …" },
                  "matched": true, "over_resolved": [] }
}
```

**The rule, from the user** (CLAUDE.md, "Time, the waterfall, and the live view", invariant 2): *the timeline is the capture window, and it is a visualization.* The scrubbable capture-history timeline spans **exactly the configured recording/retention duration** — no more, no less — grows and shrinks when that duration is reconfigured, and is itself a **compressed "sideways" overview waterfall** of the retained capture, never an empty box.

#### The horizon is the ring's, and it is named

Two retention horizons run on this server and they are **deliberately different lengths**: the IQ capture ring ([ADR-0014](adr/0014-iq-capture-ring.md); `--iq-retention`, minutes, lossless) and the tiered spectrum-history pyramid (lossy, byte-budgeted, days). A scrubber sized from the longer one lets the user scrub to a time the ring has already overwritten — it **promises capture that no longer exists**, and it looks right while it does so. `window` is therefore the ring's, and `window.horizon` (`"iq-ring"`) says which, so nothing downstream has to assume it.

- `retention_s` is the **configured** window and is the band's span exactly; `t1_s` is the live edge of capture and `t0_s = t1_s − retention_s`. `span_s` equals `retention_s` by construction, and is served so a client never computes it.
- `buffered` is what the ring currently *holds*, and sits **inside** the band. It never resizes it: a ring ten seconds into a ninety-second retention is a mostly-empty ninety-second capture window, not a ten-second one, and a band sized to what it holds would grow under the user as the ring filled. `null` when the ring holds nothing.
- The live edge is the ring's newest sample, falling back to the spectrum history's newest frame while the ring is still empty. Never wall-clock: a replay or time-compressed scene runs on its own clock (T-125), and a band anchored to `now` would place its capture in the future.
- With no ring, no retention, or no live edge, `t0_s`/`t1_s`/`span_s` are `null` and `reason` says why. **A missing capture window is drawn as unknown, never as a default span.**
- `GET /api/navigation`'s `time.latest_s` is the **history** horizon and is not used here. `GET /api/iqbuffer` reports the same ring status in full (segments, quota, eviction); this route is the window alone, plus the picture on it.

#### The overview is a measurement, so it is made here

The band is a data display, so something must decide what each drawn cell shows — and by T-334's rule, *mapping a time to a pixel is presentation, choosing which value represents an interval is a measurement.* The pyramid alone cannot serve this particular picture: its ladder **couples the axes**, so the tier whose cells are coarse enough in frequency for a thin strip's few rows (100 kHz) has one-day time cells. No single level is fine in time and coarse in frequency at once.

So the tier is chosen from the **time** axis — the coarsest tier whose cells are no larger than one drawn column — and its grid is folded onto exactly `columns × rows` cells laid on the capture window (`hk_store::RegionHistory::overview`). Two consequences:

- The grid **is** the window. Cell 0 starts at `window.t0_s` and cells are `span_s / columns` by `(hi_hz − lo_hz) / rows` — sizes no pyramid tier has.
- `matched` is always `true` and `over_resolved` always `[]`, unlike `/api/history`: because the fold happens here, the client is never handed more cells than it can draw. `reduced_from` reports the source grid it was folded from, and `src_t_cell_s`/`src_f_cell_hz` the tier's own cells. When `src_t_cell_s` exceeds `t_cell_s`, one measured value **repeats** across columns — T-334's safe direction, stated rather than hidden.

**Only statistics that fold exactly are carried.** The max of max-holds *is* the max-hold; the max of `occupancy_max` is the peak occupancy; `frames` sum; `coverage` is the **extent-weighted** sum of the source cells' coverage — each weighted by the fraction of the output cell it overlaps — i.e. the observed fraction of the output cell's **own** extent. **It is not a mean over source cells** (T-419): this grid is fractional (`t_cell_s = (t1 − t0)/nt`) and folds every source cell that *overlaps* an output cell, so a mean let a 1 % overlap count as much as a 100 % one, and let a single observed source cell report a whole collapsed column (`rows=1`, `columns=1`) as fully covered. Folding must never *lower* a measurement and never *raise* coverage; where the output grid is an exact coarsening of the source grid the two rules agree exactly. A percentile (`p_low_db`, `floor_db`) cannot be folded from cell values at all, so it is **not offered** rather than approximated. `range_db` is the grid's own observed range of `max_db` — picking a colour scale from whatever numbers you happen to hold is a measurement too — and is `null` when nothing was observed.

`null` in `max_db`/`occupancy_max` is **not observed**, never quiet (C26). `observed_cells` counts the cells something was folded into.

#### A populated finer tier answers when the coarsest one is empty (T-426)

That coarsest tier is **preferred, not required**. *Adequate is a ceiling, not a target*: a tier is adequate when its cells are no larger than one drawn column **and** its grid fits the cell budget, and every tier finer than the coarsest adequate one meets both conditions too. So the adequate tiers are a run of the ladder, and the read takes the coarsest **that holds anything** — reading finer costs cells, but serving an empty picture over data the next tier down is holding costs the user the picture.

That is not hypothetical. Over the default capture window with `rows = 1` (the survey strip's fold, and `/api/coverage`'s) the preferred tier is level 1, whose cells are 60 s — and a level-1 cell exists only once a level-0 block has **sealed**, 2 s after an epoch-aligned minute boundary. A server therefore served **no shade at all for its first minute of life** while level 0 had held 1 s cells the whole time: the user's *"we have it but didn't render it"* bug (CLAUDE.md, 2026-09-16), one layer under the black Live waterfall.

**`level` and `src_t_cell_s` report the tier that actually answered**, on both routes — a silent fallback would trade one lie for another. Reading the pair:

- `src_t_cell_s` **smaller** than the drawn cell is *more* resolution than the picture asked for, folded down onto the grid you requested. Nothing is invented and no warning is needed; only the opposite direction (`src_t_cell_s` > `t_cell_s`, a measured value repeating across columns) is a claim about the picture, and it keeps its statement above.
- The walk is only ever **downward**. Coarse tiles are rolled up from fine ones, so a coarser tier can never hold what a finer one lacks; the converse *can* happen — the byte budget evicts the finest tiles first — and the preferred-first order already covers it.
- When **no** tier holds anything, the preferred tier's empty answer stands and `level` names it, so an honestly empty window is still honest about its resolution.

#### `coverage` — the record-derived plane, and why `grid.coverage` is not it (T-423)

`grid.coverage` and the top-level `coverage` block answer **different questions**, and only the second one decides grey.

| | `grid.coverage[i]` | `coverage.any.cells[i].state` |
|---|---|---|
| Derived from | **frames** the spectrum-history pyramid still holds | **records**: the IQ ring journal's segments and the observation log's dwell/sweep windows |
| `0` / `"unobserved"` means | no frames here — *either* nothing looked *or* the byte budget evicted what it saw | no surviving record covers this cell: nothing looked |
| Answers | how much of this cell the pyramid can still draw | whether the front end was **tuned** to this cell, then |

A cell with no frames but a covering tune record is *sampled, level not retained* — which a client must draw differently from grey, and could not tell apart from `grid.coverage` alone. That confusion is the bug T-405's survey bar and T-411's time navigator both had, one axis each.

The plane is laid on **exactly the same axes as `grid`** — `nt = columns` time rows × `nf = rows` frequency cells, row-major, earliest row and lowest frequency first — so a client indexes one array with the other's index. `coverage.grid` restates those axes and `aligned` says whether they still match; the per-(t, f) cap in the rasteriser can reduce the time axis, and a realised grid is never implied. Cells carry no `shade` key: this route serves its own levels in `grid.max_db`, and a `shade: null` would read as the *sampled, level not retained* claim above.

Everything else — the cell states, `devices[]`, `any`, `horizon`, `sources` — is exactly [`/api/coverage`](#get-apicoverage--the-coverage-map-grey-means-genuinely-unobserved-t-368)'s, from the same computation, so the two routes cannot disagree about what was sampled.

#### The band-collapsed series, and why the response states its own semantics (T-342)

`rows=1` (the default) is the **band-collapsed activity-vs-time series**: one value per time step over a whole region. It is the sibling of [`/api/analysis/strongest`](#get-apianalysisstrongest--strongest-signal-in-a-band-t-079) — "strongest in a band", kept server-side — laid out along a time axis the way [`/api/floor`](#get-apifloor--calibrated-floor-vs-time-t-021-space-050)'s `max_steps` lays out a floor track. The pyramid cannot serve it directly at any level: its coarsest **frequency** cell is 100 kHz, so no `max_f` collapses a MHz-wide band into one column. The fold does, and it is the same fold transposed that gives `/api/coverage` its survey-strip shade (`nt = 1`: one max-hold per frequency cell over the whole window).

It is served with its semantics **on the wire**, in `grid.semantics`, because a number whose statistic and scale are unstated is one a consumer will re-derive or misread — which is exactly how this measurement came to live in `ui/src` in the first place, as a max over every frequency cell normalised against the response's own range.

| Field | Says |
|---|---|
| `semantics.fold` / `semantics.rule` | The statistic — **max-hold** — and both its consequences: folding further never lowers a value (so a brief emission survives the collapse), and **the max of nothing is unobserved, not zero** |
| `semantics.series.<name>` | Per series, since they are not all max-holds: `statistic` (`max-hold`, `max`, `mean`, `sum`), `scale`, and what an unobserved cell reads as |
| `semantics.unobserved_rule` | That `null` is *never observed*, never *quiet*, and **not the bottom of the scale** |
| `grid.unit` / `series.*.scale` | The scale of `max_db`/`range_db`, carried from the source grid rather than assumed: `"dbfs"` (uncalibrated, relative to ADC full scale) or `"dbm"` (at the antenna port), densities per Hz — `"dbfs-per-hz"` / `"dbm-per-hz"` spelled out in `scale` |

**The budget is honoured exactly, and `resolution.budget` says what became of it.** `columns` is a *time-axis* budget, the same idea as `/api/floor`'s `max_steps` and not a page size: asking for more columns than the window has source cells never truncates the window. Per axis, `requested`, `served` (always equal — the fold lays the grid on the window itself), `source_cells`, and `replicated`, which is `true` when there were fewer source cells than cells asked for and one measured value therefore **repeats** across the extras — T-334's safe direction, so a neighbouring pair of equal values can be told from two measurements that agreed.

#### The detail claim

`resolution` is T-334's block with T-341's three-valued `source` ([Live-IQ detail versus overview](#live-iq-detail-versus-overview-t-341)). The timeline's *horizon* is the ring's; its *pixels* are the pyramid's, so it claims `"spectrum-history"` — or `"survey-overview"` for a span no single capture window could hold — and **never `"live-iq"`**, exactly as `/api/history` does not. `resolution.horizon` repeats `"iq-ring"` beside it, because those are two different questions with two different answers: *which retention does this picture span* and *which tier drew it*.

### `GET /api/coverage` — the coverage map: grey means genuinely unobserved (T-368)

Query parameters: `f_lo`&`f_hi` (Hz, **required** — the band to report on), `cells` (1…4096, default 256), `rows` (1…4096, default 1 — the **time** axis, T-423), `t0`&`t1` (Unix s, given together; default the capture window this server holds).

**The rule, from the user** (CLAUDE.md, "Time, the waterfall, and the live view"):

> **The waterfall shows the data that exists for the selected (time, frequency); grey means genuinely unobserved.** The view renders whatever samples are actually available for the current time-and-frequency selection, and greys only cells that were truly never observed — never a fixed-size grey placeholder. … This requires the backend to keep a **coverage map derived from the SDR configuration/tune history** — for each interval, which centre/span/rate (and which device) was active — so observed-vs-unobserved is computed from what was actually sampled, and the frequency navigator's survey view is built from that same coverage.

[`/api/navigation`](#get-apinavigation--the-achievable-centre-span-grid-t-341) stopped the view *claiming* detail the front end never captured. This is the other half: the view may *show* what the front end did capture, and must grey only what it did not. Between them sits the failure this route exists to prevent — **painting never-observed spectrum as quiet**, which invents an absence-of-signal finding out of an absence of measurement.

```jsonc
{
  "region": { "lo_hz": 88000000.0, "hi_hz": 108000000.0 },
  "window": { "t0_s": 1789300320.0, "t1_s": 1789300920.0, "span_s": 600.0,
              "source": "capture-window" },   // or "requested" when t0/t1 were given
  "grid":   { "cells": 4, "rows": 1, "requested_rows": 1,     // `rows` is the REALISED time axis
              "f_lo_hz": 88000000.0, "f_cell_hz": 5000000.0,
              "t0_s": 1789300320.0, "t_cell_s": 600.0,
              "order": "row-major: cells[t * cells + f], earliest row first, low frequency first" },
  "devices": [{                                // one entry per front end that actually sampled here
    "device": "hackrf:0000000000000000a06063c8234e925f",
    "named": true,                             // false for the "unknown" and "any" labels
    "observed_cells": 2, "unobserved_cells": 2, "unknown_cells": 0, "excluded_cells": 1,
    "observed_fraction": 0.5,
    "cells": [
      // 1. observed, and there was energy
      { "state": "observed", "spans": 1, "observed_s": 600.0, "duty": 1.0,
        "last_s": 1789300920.0, "center_hz": 100800000.0, "sample_rate_hz": 2400000.0,
        "shade": 0.87 },
      // 2. observed, and it was quiet — a real, reportable finding
      { "state": "observed", "spans": 1, "observed_s": 600.0, "duty": 1.0,
        "last_s": 1789300920.0, "center_hz": 100800000.0, "sample_rate_hz": 2400000.0,
        "shade": 0.0 },
      // 3. never observed — no claim either way. This is the grey cell.
      { "state": "unobserved" },
      // observed, but the history keeps no level here: neither grey nor the ramp's bottom
      { "state": "observed", "spans": 2, "observed_s": 41.5, "duty": 0.069, "analysed_s": 41.5,
        "last_s": 1789300880.0, "center_hz": 104000000.0, "sample_rate_hz": 2400000.0,
        "shade": null },
      // 5. sampled, and DELIBERATELY excluded from analysis (T-595): the DC/LO notch. Every
      // measurement key an observed cell has, plus `analysed_s: 0.0` saying why it is not
      // simply "observed". NOT grey: the radio was here and the history holds rows.
      { "state": "excluded", "spans": 1, "observed_s": 600.0, "duty": 1.0, "analysed_s": 0.0,
        "last_s": 1789300920.0, "center_hz": 100800000.0, "sample_rate_hz": 2400000.0,
        "shade": 0.41 }
    ]
  }],
  "any": { "device": "any", "named": false, "observed_cells": 3, "unobserved_cells": 1,
           "unknown_cells": 0, "cells": [ … ] },
  "horizon": { "oldest_record_s": 1789214520.0,  // null when nothing here holds a tune record
               "recording_began_s": 1789214400.0, // T-507: null when nothing here ever recorded
               "forgotten": null,                 // or why the past before it is unbounded
               "as_of_s": 1789214880.0,           // T-532: how far FORWARD this answer reaches
               "unknown_from_row": 0, "unknown_rows": 0, "rows": 1,
               "rule": "a row wholly before `oldest_record_s` has no surviving tune record, so its unsampled cells are \"unknown\" (we no longer know whether we looked) - UNLESS the row is also wholly before `recording_began_s` and nothing is `forgotten`: before this installation recorded anything, nothing looked, and the cell is \"unobserved\". …",
               "state_rule": "\"unknown\" carries no measurement keys, exactly like \"unobserved\", and must be drawn as neither grey nor a level …" },
  "sources": [
    { "kind": "iq-ring",         "spans": 37, "named_spans": 37, "device_known": true, "available": true },
    { "kind": "observation-log", "spans": 12, "named_spans": 12, "device_known": true, "available": true },
    { "kind": "open-dwell",      "spans":  1, "named_spans":  1, "device_known": true, "available": true }  // T-596: the dwell in flight
  ],
  "shade": { "fold": "max-hold", "rule": "max-hold: a cell is the maximum of the source cells folded into it, … the max of nothing is unobserved, not zero",
             "statistic": "max-hold over the whole window, per frequency cell",
             "scale": "dbfs-per-hz", "range_db": { "lo": -138.2, "hi": -91.0 },
             "normalisation": "0 at `range_db.lo`, 1 at `range_db.hi`, linear in dB and clamped",
             "level": 0, "src_t_cell_s": 1.0, "src_f_cell_hz": 6250.0,   // T-426: the tier that ACTUALLY answered
             "level_rule": "the tier that ACTUALLY answered: the coarsest tier whose time cells are no larger than one drawn cell is preferred, and a populated finer tier answers when it is empty …",
             "unobserved": "an unobserved cell carries no `shade` key: the max of nothing is unknown, not zero, …" },
  "resolution": { "source": "survey-overview", "live": false, "statement": "…",
                  "served_span_hz": 20000000.0, "max_live_span_hz": 20000000.0,
                  "grey_rule": "grey a cell if and only if its state is \"unobserved\"; \"unknown\" is not grey and not a level — draw it as a fourth thing (hatching, per T-413)",
                  "shade_rule": "shade is the max-hold over the window, normalised over `shade.range_db`; it never decides observed-versus-unobserved" }
}
```

#### Five states, and none of them can be spelled as another

| State | Meaning | On the wire |
|---|---|---|
| 1 | observed, and there was energy | `"state": "observed"` with a high `shade` |
| 2 | observed, and it was **quiet** — a finding | `"state": "observed"` with a low `shade` |
| 3 | **never observed** — no claim either way | `"state": "unobserved"`, **and no measurement keys at all** |
| 4 | **we no longer know whether we looked** (T-423) | `"state": "unknown"`, and no measurement keys either |
| 5 | observed, and **deliberately excluded from analysis** (T-595) | `"state": "excluded"`, with every measurement key an `"observed"` cell has and `"analysed_s": 0.0` |

The pair that gets collapsed is 2 and 3, and collapsing them is how a view comes to report "nothing here" about spectrum nothing ever looked at. So an unobserved cell carries **no `shade`, no `duty`, no `observed_s`** — not `null` ones. That is stronger than a nullable number, because there is no field a client can read as zero: the absence is structural. It is the same rule as `bias_tee: "unknown"` ≠ `"off"` — **nothing said is never permissive** — and it holds in the type as well as the JSON: `hk_store::coverage::Coverage::of` refuses to mint an observation out of a zero span count or a zero sampled duration, and hands back `Coverage::Unobserved` instead.

`shade: null` on an **observed** cell is a different thing again: sampled, but the spectrum history keeps no level for it. A client draws that differently from grey and differently from the bottom of the ramp. **Grey is `state == "unobserved"` and nothing else**, which is what `resolution.grey_rule` says in the response.

#### The fifth state: `"excluded"` means *we sampled it and deliberately did not analyse it* (T-595)

The receiver excludes its own DC/LO-leakage notch — ±15 kHz around the tuned centre, `hk_detect::DcRule`'s tolerance, the same number `dc_excluded_hz` reports on the spectrum header — from **detection**. The observation log records that as `records[].window.dc_excluded`, and until T-595 the coverage fold read the hole as an absence of *sampling*: the notch contributed no span at all, so it rasterised as `"unobserved"`.

It is not unobserved. The ADC digitised it, the FFT produced bins for it, and the spectrum-history pyramid keeps rows right across it. **T-588 measured the consequence**: over a sweep of 1 966 080 cells, 1 212 cells held a measurement and read `unobserved` — and *all 1 212 were the DC notch*. Both halves of the invariant broke at once: data that exists was not shown, and grey stopped meaning genuinely unobserved.

Why it went unseen for so long: inside the IQ ring's retention the **ring journal**'s segments cover the whole tuned window with no notch (the ring holds the samples, DC included, so a client can re-analyse them), and those spans paper over the hole. Past the ring horizon only the observation log is left, and the stripe appears. A test that looks only at recent history passes while the defect is intact.

The fix gives the notch its own mark rather than making the observation log lie in the other direction by declaring it analysed:

- **It is an observation, not an absence.** `"excluded"` carries every measurement key `"observed"` carries — `spans`, `observed_s`, `duty`, `last_s`, `center_hz`, `sample_rate_hz`, `shade` — because all of them are true. It is not a fourth kind of ignorance; `hk_store::coverage::Coverage` still has exactly two variants, and *excluded* is a property of the observation (`analysed_ns == 0`), not a value beside `Unobserved`.
- **`analysed_s` is the number behind the word.** Every observed cell now carries it: of `observed_s`, how many seconds the analysis actually ran on. `analysed_s == 0.0` *is* `"excluded"`, so a client can check the claim instead of taking it. A cell whose extent is partly analysed (a coverage cell wider than the notch, or a coarse tile that swallows it) reports what it is — `0 < analysed_s <= observed_s` — and reads `"observed"`: a 30 kHz exclusion is not a claim about a 200 kHz cell.
- **Draw the measurement, mark it distinctly, never grey.** `resolution.grey_rule` says so in the response. The canvas draws the level on the same ramp with a vertical-rule ink over it (`ui/src/surface/cellrule.ts`, the seventh cell state) — a mark ruled along the *frequency* axis, which is the axis the exclusion is a stripe on.
- **It can change at the ring horizon, honestly.** Inside the ring the same cell reads `"observed"`, because the raw samples are there to analyse; past it, `"excluded"`, because the only surviving evidence is a record that says the analysis skipped it. Each is the truth about what we can still say.

#### The time axis: `rows` (T-423)

`rows` is the **time** budget, the twin of `cells`. It defaults to `1`, which is one row over the whole window and is T-368's original answer unchanged: *was this band sampled anywhere in this window*. That is a **column**, and a view drawing a waterfall needs a **cell** — *was it sampled __then__*. Without the axis, a band the radio watched for ten seconds of a minute came back `observed` for the whole minute, and both the survey bar (T-405) and the time navigator (T-411) drew a cell the radio was demonstrably tuned away from as *sampled, level not retained* rather than grey.

- `cells` arrays are **row-major**: `cells[t * grid.cells + f]`, earliest row first, low frequency first. `grid.order` says so in the response.
- `grid.rows` is the **realised** row count and `grid.requested_rows` what was asked for. The rasteriser bounds the product `rows × cells`, and reduces the **time** axis when it must — so a realised resolution is never implied. `grid.t0_s` and `grid.t_cell_s` are the axis itself.
- Each cell's `duty` and `observed_s` are against **its own row's** extent, not the window's. So a column at `duty` ≈ 1/6 and its one covered row at `duty` ≈ 1 describe the same seconds: `observed_s` sums across rows, `duty` is re-derived, never averaged (T-419: `observed_s` is foldable, `duty` is not).
- The same rasterisation serves `devices[]` and `any`, so **the time axis is per device** and is never flattened into a merged claim (T-259/T-305).

#### The fourth state: `"unknown"` means *we no longer know whether we looked*

The horizons on this server are **deliberately different lengths and they cross**: the spectrum-history pyramid has no age limit at all (a rolling byte budget), the IQ ring holds minutes, and the observation log expires at 180 days (T-406 raised it from 30 so the coverage record outlives the pyramid it explains; the byte quota can still bind first — see below). So spectrum exists that no surviving coverage record covers — and, far more commonly, a requested window simply reaches back past every record this server still holds.

`"unobserved"` is the claim *nothing looked*. Past the record horizon nothing supports that claim, and painting it grey spells "never looked" for spectrum whose records were merely discarded. That is the same error as reporting a never-observed cell as quiet, one horizon out — so it gets its own state, and `resolution.grey_rule` says in the response that it is **not grey**. Draw it as a fourth thing; hatching is the house precedent (T-413).

`horizon` makes the claim checkable rather than asking the client to take it:

| Field | Says |
|---|---|
| `oldest_record_s` | The earliest instant **any** consulted source still holds a tune record for — `min` over the IQ ring's buffered start and the observation log's oldest surviving hour. `min`, not `max`: a row is knowable if *at least one* record reaches it. `null` when nothing here holds a record. |
| `recording_began_s` | **When this server's memory of recording begins** (T-507): `min` over the IQ ring's buffered start, the observation log's **earliest record** (its first sampled instant — not its oldest hour, which is a filing boundary up to an hour before any sample and is what `oldest_record_s` uses) and the spectrum history's own record of when it began recording (a fact it persists, so it outlives both a restart and the tiles that proved it). `null` when nothing here has ever recorded. Rows wholly before it are `"unobserved"`: before this installation recorded anything, nothing looked. |
| `forgotten` | `null`, or why the past before `recording_began_s` is unbounded: a source has **discarded** records that could reach back past it (the observation log deleted a segment by retention; or the IQ ring evicted data on a server with no spectrum history to remember when recording began). Then every row before `oldest_record_s` is `"unknown"`. |
| `unknown_from_row`, `unknown_rows` | The rows served as `"unknown"` are exactly `[unknown_from_row, unknown_from_row + unknown_rows)` — a contiguous band: wholly before `oldest_record_s`, and not wholly before `recording_began_s` (a row straddling it counts as unknown). |
| `as_of_s` | **How far FORWARD this answer's evidence reaches** (T-532): the newest instant any consulted span over *this band* ends at, clamped into the asked-for window. `null` when no record touches the band at all. |
| `rows` | The grid's realised row count, so the band can be read against it. |

**`"unknown"` is what a server recorded and lost, never the default for a young one (T-507).** Until T-507 every row before `oldest_record_s` was `"unknown"`, so a freshly started or reset server painted everything before its first sample in the fourth state — the magenta hatch covered 54 % of a live pane 3 s after a restart and faded only as the window slid past the start. A server that has never recorded has forgotten nothing; its true answer about the time before it started is `"unobserved"`. The fourth state is now exactly three cases: rows between `recording_began_s` and `oldest_record_s` (recording happened, its tune record did not survive — e.g. a short IQ ring evicted it before the observation log's once-a-minute interactive record covered it); every row before `oldest_record_s` when `forgotten` is set; and every row on a server with **no tune history at all** (no IQ ring and no observation log), which cannot say whether it looked. What `recording_began_s` cannot see is a store deleted from disk, or a spectrum history written before T-507 that had already evicted its oldest tiles: its oldest surviving block stands in, a lower bound, which can only widen `"unknown"`, never claim `"unobserved"` over a span that was recorded.

Two rules hold and are stated in the response:

- **An observed cell is never relabelled.** A surviving measurement is itself proof we looked, so it stays `"observed"` past the horizon. Only `"unobserved"` can become `"unknown"`.
- **There is a horizon at the young end too, and a client that KEEPS an answer must obey it (T-532).** A tune record is written as capture proceeds, so it stops at the newest sample: every row after `as_of_s` is served `"unobserved"` because *no record reaches there yet*, not because nothing looked. Served, that is true — nothing has happened there yet. **Held, it becomes false the moment capture continues**, and `GET /api/tiles` answers are held: a tile cache keeps a resident copy until its revalidation lane comes round. So a reader may not draw grey past `as_of_s`; the honest statement for that strip is *this answer does not reach here*. It was invisible while the finest time cell was a second — the error hid inside the cell the live edge was already in — and at the fidelity floor's 40 ms cell it is a band of grey across the newest second or two of every live pane, over rows the radio recorded and this server is serving. `as_of_s: null` is **not** "reaches everywhere": a band no record touches was never observed at any instant in it, and the answer stands as served.
- **The horizon is a time, not a band.** A discarded record takes every frequency with it, so the state applies to whole *rows*. `unobserved_cells` and `unknown_cells` are reported separately and never summed for you.

This is a **wire** state and deliberately not a third `hk_store::coverage::Coverage` variant (`docs/16` §5.4 asks for the reasoning to be stated): the fold is handed spans, which do not carry the horizon that produced them, so only the caller that *read* the records can decide it; the state is a property of a row rather than of a cell, and `CoverageGrid::unknown_rows_before` already has that shape; and `Coverage`'s two-variant design is what makes state 3 unrepresentable as state 2, which a third variant would disturb for every consumer to say something none of them could compute.

#### What a `shade` is, and against what (T-342)

A 0–1 number normalised against a range the response never named is a measurement a consumer cannot check, match or reproduce — and a strip drawn on one scale beside a waterfall drawn on another makes the same energy read as two different strengths on one screen. So the `shade` block states the whole of it: the fold (**max-hold over the window**, one value per frequency cell — the same fold [`/api/timeline`](#the-band-collapsed-series-and-why-the-response-states-its-own-semantics-t-342) applies along the other axis, via the same `hk_store::RegionHistory::overview`), the `scale` and `range_db` the ratio is relative to, the `normalisation` itself, and the constraint that matters most here: **max-hold must not turn an unobserved cell into an observed one.** An unobserved cell carries no `shade` key at all — the max of nothing is unknown, not zero, and not the bottom of the ramp.

`level`, `src_t_cell_s` and `src_f_cell_hz` name **which pyramid tier drew the shading** (T-426), the same three fields `/api/timeline`'s `resolution` carries and from the same read. They are on the wire because the tier is not a function of the window alone: the coarsest tier whose cells fit one drawn cell is *preferred*, and when it holds nothing a populated finer tier answers instead — see [A populated finer tier answers when the coarsest one is empty](#a-populated-finer-tier-answers-when-the-coarsest-one-is-empty-t-426), which is also the bug this route showed most plainly (no shade for the first minute of a server's life, over data level 0 was holding all along). `src_t_cell_s` smaller than `grid.t_cell_s` is more resolution than was asked for, folded down; `level_rule` says so in the response. All three are `null` only when this server has no spectrum history at all.

#### Device-local, never unioned

Coverage is a fact about **one front end**. `devices[]` holds one grid per radio that actually sampled in the region, and two radios covering disjoint ranges come back as two grids, each unobserved exactly where the other looked — never merged into a claim that either one saw both (T-259/T-305: device-local physics reads the device). `"unknown"` is its own device, not a wildcard: a span whose record did not name the radio is evidence that *something* looked, never that a *particular* front end did, so it never satisfies a query for a named one. `any` is the deliberate union and is labelled `"any"` with `"named": false`, so nothing can mistake it for one radio's coverage.

#### Where the map comes from: provenance already written

Nothing new is journalled for this. Two records already say "for each interval, which centre/span/rate was active", and one of them also says which device:

| Source | Interval | Centre/span/rate | Device | Horizon |
|---|---|---|---|---|
| IQ ring journal ([`/api/iqbuffer`](#rolling-iq-capture-buffer-t-157) segments, ADR-0014) | yes | yes | **yes** (`device_id`) | the ring's retention |
| observation log (`DwellRecord`/`SweepRecord`, ADR-0012 §1) | yes | yes (`ObservedWindow`) | **yes** (`device_id`, T-378) | 180 days / 2 GiB, whichever binds (T-406) |
| the dwell **in flight** (`open-dwell`, T-596) | yes | yes (`ObservedWindow`) | **yes** (`device_id`) | from the settled tune to the newest sample |

The ring journal opens a new segment on **every** provenance change, so retunes are segment boundaries by construction — it is already a tune history. **T-378** put the same `device_id` on the observation log's records — the source's own `DeviceInfo::device_id`, the one value the baseline chain key and the history source key are also hashed from — so the long horizon is device-local too, and coverage over the whole retention answers *"did **this** front end look here"* rather than only *"did anything"*.

**The live edge has a third source, because a record appears only when a dwell *seals*** (T-596). An interactive dwell is written when the tune changes or after 60 s of a steady tune, so for up to a whole dwell after every retune the observation log says nothing about the band the radio is sitting on and measuring. With an IQ ring that gap is covered by the ring journal; **with the ring refused it is covered by nothing**, and T-588 measured the consequence — 18 rows (18 s) of `max_db` served `"unobserved"`, data that exists drawn grey. The ring had been refused *because the disk was full*, which on a portable device is the field failure mode rather than an exotic configuration. So the dwell in flight is served as coverage too, under `sources[].kind == "open-dwell"`. It is **not a weaker claim and carries no mark of its own**: the samples are sampled, the analysis has run over the rows that exist, and only the bookkeeping is outstanding — so it rasterises through the same mapping as a sealed record, declares the same `dc_excluded` notch (`"excluded"`, T-595), and a cell's state does not change when the seal catches up. What is new is the *source*, so a client can see which evidence carried the live edge. It never appears in `GET /api/observations`: a provisional record must not reach the paths that count sealed visits. **And it speaks forward only.** The open dwell is clipped to start at `horizon.oldest_record_s`: it is coverage over the interval it has actually run — its own start to the live edge — and a *provisional, unsealed* record besides, so it may extend this answer forward but never overrule the `"unknown"` the same answer publishes for rows before the record horizon (T-413/T-507). *We do not know whether we looked* is not *we did*, exactly as `Coverage::Unobserved` is not quiet. With no surviving record anywhere (`oldest_record_s` null) the horizon admits nothing and the open dwell contributes nothing. This costs the live edge nothing: the gap after a retune lies after the last sealed record by construction. **Forward, though, it does move the horizon**: the open dwell is folded into the spans *before* `horizon.as_of_s` is taken (T-532), so `as_of_s` reaches the live edge the planes beside it are drawn from rather than stopping at the last seal — the forward horizon and the plane come from the same spans and must not disagree. The asymmetry is the rule: an unsealed record may say how far forward **this answer** reaches, and may not restate what **this server** has forgotten.

A record that names no device — every record written before T-378, and any source that states no identity — stays `"unknown"`, and is **never** read as the radio that happens to be running now. `sources[]` therefore reports two numbers per record kind: `spans`, how many it contributed, and `named_spans`, how many of those actually named a front end. `device_known` is the measured `named_spans == spans`, not a declaration about the record kind, so a log still holding pre-T-378 lines says so. A source with no spans still appears, so a client can tell *this record had nothing here* from *this record was not consulted*. **`available` means the source can actually contribute evidence, not that a handle is wired** (T-640): an IQ ring whose allocation was **refused** for lack of free space still answers `GET /api/iqbuffer` while holding no journal, and reports `available: false` here. That distinction decides a coverage state — with no tune history at all, a row's unsampled cells are `"unknown"`, not `"unobserved"`, because *the radio did not look* is a claim nothing supports. A ring still **allocating** reports `false` for the same reason: nothing is buffered until it completes.

### `GET /api/tiles` — one tile of the unified surface, at independent `(level_f, level_t)` (T-438, [docs/16](16-coverage-tile-pyramid-and-full-spectrum-view.md) §7 step 5 / §8)

Query parameters: `level_f`&`level_t`&`f_index`&`t_index` (**required**, integers ≥ 0), `scheme` (`view` — the default — `overview`, or a store scheme id), `device` (`any` by default, or a device id), `cells` (8…256, default 256), `planes` (`json` — the default — or `f16`; see [below](#the-measurement-plane-is-served-as-binary16-on-request-t-533)), `client` (optional; who is asking, for the per-client share of the in-flight cap — see [Cost, and the two caps](#cost-and-the-two-caps)).

One route serves every viewport — the panes, the zoomable minimap and the live edge — because they are **projections of the same pyramid**, and one route is what stops them ever disagreeing on one screen ([docs/16](16-coverage-tile-pyramid-and-full-spectrum-view.md) §7 step 5, strengthened by §8: there is no live-versus-history split left to keep consistent).

**Caching (T-574).** A tile's own `sealed` field says whether its time extent has fully passed the pyramid's watermark — no later frame can still land inside it, so its bytes can never change again. A **sealed** response carries `Cache-Control: public, max-age=31536000, immutable` and an `ETag` (a CRC-32 of the exact response bytes, quoted); a repeat `GET` with a matching `If-None-Match` gets **`304 Not Modified`** with an empty body and the same `ETag`/`Cache-Control`. A tile that is **not** sealed — most of all the growing live edge, whose bytes change on the next ingest — keeps the route's usual `Cache-Control: no-store` and carries **no `ETag` at all**, so a cache can never answer it from a stale copy or a 304 that has gone out of date. `sealed` is derived from the pyramid's own state (the watermark against the tile's own extent), never from the tile's age or a guess, and it is the same fact `tiles.rs` uses internally to decide sealed-vs-derived storage.

```jsonc
{
  "sealed": true,
  "key": { "device": "any", "device_named": false, "scheme": "view",
           "level_f": 3, "level_t": 5, "f_index": 139, "t_index": 218427, "cells": 256 },
  "extent": { "f_lo_hz": 1779200000.0, "f_hi_hz": 1792000000.0, "f_cell_hz": 50000.0,
              "t0_s": 1789300736.0, "t1_s": 1789309926.0, "t_cell_s": 32.0, "nt": 256, "nf": 256 },
  "axes": { "frequency": { "levels": 20, "max_level": 9, "cell_hz": 50000.0, "tile_hz": 12800000.0 },
            "time":      { "levels": 15, "max_level": 1, "cell_s": 32.0, "tile_s": 8192.0 },
            "store_node": null,
            "independent": "level_f and level_t are independent coordinates: …",
            "readable": "`max_level` is a CEILING ON READING, per axis, and it is a BOX: …" },
  "grid": { "nt": 256, "nf": 256, "t0_s": …, "t_cell_s": 32.0, "f_lo_hz": …, "f_cell_hz": 50000.0,
            "encoding": { "planes": "json", "order": "row-major: time then frequency, …", "rule": "…" },
            // …or, with `?planes=f16`, `max_db` is ABSENT and this stands in its place:
            //  "planes": { "max_db": { "type": "f16", "byte_order": "little-endian",
            //                          "transfer": "base64", "cells": 65536, "bytes": 131072,
            //                          "scale": "dbfs-per-hz", "absent": "nan", "data": "…" } },
            "max_db": [-102.4, null, "…"], "occupancy_max": ["…"], "coverage": ["…"], "frames": ["…"],
            "cells": 65536, "observed_cells": 4096, "range_db": { "lo": -138.2, "hi": -91.0 },
            "unit": "dbfs", "percentiles": "unknown: a de-welded fold cannot split …",
            "semantics": { "…": "…" } },
  "coverage": { "encoding": "plane-table-rle",
                "grid": { "nt": 256, "nf": 256, "t0_s": …, "t_cell_s": 32.0, "f_lo_hz": …,
                          "f_cell_hz": 50000.0, "aligned": true, "order": "row-major: …" },
                "states": ["unobserved", "observed", "unknown", "excluded"],
                "planes": [ { "runs": [0, 12288, 1, 53248], "cells": 65536, "uniform": null,
                              "observed_cells": 53248, "unobserved_cells": 12288,
                              "unknown_cells": 0, "excluded_cells": 0, "observed_fraction": 0.8125 } ],
                "any": { "device": "any", "named": false, "plane": 0 },
                "devices": [ { "device": "hackrf:0000…925f", "named": true, "plane": 0 } ],
                "selected": { "device": "any", "named": false, "present": true, "plane": 0,
                              "rule": "…" },
                "horizon": {"…": "…"}, "sources": ["…"],
                "rule": "record-derived: …", "encoding_rule": "…", "per_cell_metadata": "…" },
  "shadow": { "encoding": "column-runs", "runs": 3,
              "f": [17, 18, 19], "row": [0, 0, 0], "rows": [256, 256, 96],
              "last_db": [-96.5, -101.2, -88.0],
              "last_t_s": [1789300620.0, 1789300620.0, 1789303808.0], "src": [1, 1, 0],
              "fill": [0, 0, 1], "fills": ["forward", "backward"], "backward_runs": 1,
              "sources": [ { "from": "this-tile", "level": 2, "statement": "…" },
                           { "from": "before-tile", "store": "spectrum-history", "level": 1,
                             "f_cell_hz": 12500.0, "t_cell_s": 60.0 } ],
              "edge_s": 1789309800.5,
              "search": { "store": "spectrum-history", "before_s": 1789300736.0,
                          "searched_from_s": 1788912000.0, "columns_found": 2, "unsearched": [],
                          "stages": [ { "level": 1, "from_s": …, "to_s": …, "source_cells": 3072,
                                        "found": 2, "skipped": false }, "…" ],
                          "source_cells": 5120, "chunks": 3, "build_ms": 0.8, "rule": "…" },
              "rule": "the LAST-KNOWN tier (docs/adr/0020), NOT a measurement of the row it is drawn on. …" },
  "resolution": {
    "source": "spectrum-history", "live": false, "statement": "…",
    "answered": { "level": 2, "levels": 64, "f_cell_hz": 25000.0, "t_cell_s": 900.0,
                  "exact_node": false, "store": "view-lattice" },
    "candidates": [2, 3, 4], "tried": [2],
    "fold": { "frequency": { "source_cell": 25000.0, "tile_cell": 50000.0, "source_cells": 512,
                             "served": 256, "direction": "folded", "replicated": false },
              "time":      { "source_cell": 9.0e11, "tile_cell": 3.2e10, "source_cells": 10,
                             "served": 256, "direction": "replicated", "replicated": true },
              "rule": "the served grid is ALWAYS the tile's own cells x cells; …" },
    "budget": { "max_source_cells_per_lock": 500000, "max_source_cells_per_tile": 2000000,
                "statement": "these bound WORK, never resolution: …" },
    "grey_rule": "grey is decided by `coverage`, never by this block: …"
  },
  "cost": { "build_ms": 11.6, "source_cells": 65536, "chunks": 1,
            "in_flight": 1, "in_flight_limit": 4, "in_flight_share": 2, "in_flight_held": 1,
            "clients": 2, "client": "5f2c…", "reserved": 0, "fair_share": true,
            "statement": "…" }
}
```

#### The key, and what `device` and `scheme` do in it

[docs/16](16-coverage-tile-pyramid-and-full-spectrum-view.md) §8.3 named four parts — `(level_f, level_t, f_block, t_block)`. §6.3 already required a fifth, and **retrofitting a key is the expensive kind of change**, so both extra parts are here from the start.

- **`scheme` is the lattice the address is expressed in — and, since T-439, which store answers it.** It is what makes *"no such node"* an answerable question rather than a theoretical one. The two halves cannot come apart: expressing an address in the view lattice while reading scheme 1's ladder is the gap T-438 left, where the off-diagonal nodes had nothing behind them. `resolution.answered.store` names the pyramid that answered — `view-lattice` or `spectrum-history`.
  - `scheme=view` (default) is the de-welded view lattice. Node `(0, 0)` is the open pyramid's **own level-0 cell** and each axis doubles **independently**, so every `(level_f, level_t)` inside the axes is a node. Frequency runs up to a tile wide enough to put 1 MHz–6 GHz in two tiles; time up to a tile a month tall (§6.2's V7 corner, kept).
  - **A run opens a view-scheme pyramid and the live chain writes its finest node** (T-439, `PipelineSettings.view_history`), so `scheme=view` addresses a real 8 × 8 lattice — `(level_f 0, level_t 3)` is a node with tiles in it, not a fold out of a ladder's diagonal. On a server with no view pyramid open, `scheme=view` still resolves against the spectrum-history pyramid exactly as it did before, and `answered.store` says so.
  - **`scheme=overview` is the second tier** (T-505): the same de-welded construction, anchored at the **spectrum-history** pyramid's level-0 cell and answered by that pyramid, whichever store `scheme=view` resolves to. It exists because *one lattice cannot be both the display stream's own bin at its floor and device-wide over the record horizon at its ceiling* — see [the two tiers](#the-two-tiers-the-honesty-tiers-made-real-in-the-tile-source-t-505) below.
  - `scheme=<n>` addresses a store scheme's own levels, read off the geometry by T-434's `Geometry::f_axis`/`t_axis`. **A welded ladder is the *diagonal* of its own lattice**, so `(level_f 0, level_t 3)` on scheme 1 has no node and is a `404` that says so — never a silent snap to a level whose time cell is a day.
  - `axes.store_node` is the store level whose cells are *exactly* this tile's, or `null` when the tile sits off the ladder's diagonal and is therefore folded rather than read whole.
- **`device` is whose coverage decides this tile's grey.** Coverage is device-local (T-259/T-305, §6.3), so it belongs in the key and never in a cell. `any` is the union and keeps `device_named: false`, so a merged plane can never wear one radio's identity; `coverage.selected` echoes the choice, and `present: false` says a named front end contributed no record over this tile — a coverage answer, not a missing one.

#### The two tiers: the honesty tiers made real in the tile **source** (T-505)

`axes.{frequency,time}.max_level` bounds level **indices**, never cell **size**, and `servable` reasons about *ratios* (tile span over source cell), so it is blind to absolute size. **A floor N doublings finer therefore shrinks the coarsest *addressable* tile by exactly 2^N**, and no lattice depth gives it back — T-501 swept `f_levels × t_levels` in `2..=10` at two floors and the declared ceiling is identical index for index on every depth. The binding constraint is *work*: a tile's source grid is `tile_hz / f_cell` by `tile_s / t_cell` over the store's **coarsest** level, so reach is proportional to that level's absolute cell size, and a store whose finest cell is the display STFT's own bin genuinely cannot back a device-wide tile at any price.

Measured on real pyramids (`hk_api::tiles::the_overview_tier_reaches_past_the_whole_surface_whatever_the_view_floor_is`):

| lattice's store | node (0, 0) | ceiling | coarsest addressable tile |
|---|---|---|---|
| view pyramid, T-439's floor | 6250 Hz × 1 s | `(9, 1)` | 819.2 MHz × 512 s |
| view pyramid, **the shipped display-bin floor** | 2343.75 Hz × 40.106667 ms | `(9, 1)` | **307.2 MHz × 20.5 s** |
| view pyramid, a 4096-bin display patch | 585.9375 Hz × 40.106667 ms | `(9, 1)` | **76.8 MHz × 20.5 s** |
| **scheme 1 — `scheme=overview`** | 6250 Hz × 1 s | **`(11, 14)`** | **3276.8 MHz × 48.55 days** |

The middle rows are a measured outage: at the shipped floor a 6 GHz × 30-minute minimap enumerates **1780** addresses instead of 32 — **7031** if the display is patched to 4096 bins — behind this route's four-slot in-flight cap, and nothing arrives. So the client draws each viewport from the tier that can answer it — `scheme=view` for the tuned window at the resolution the front end measured, `scheme=overview` for wide-and-long viewports — and **the pane states which tier it drew from** (`ui/src/surface/panes.ts`, `PaneStatus.tier`). `ui/test/surface-lattice.test.ts` pins the counts per viewport at a thirty-minute horizon against a stated budget of 100 tiles, at both floors, and pins the 1780 and the 7031 as the non-vacuity cases; `hk_pipeline`'s `live_edge_tiles::the_shipped_floor_is_the_lattice_the_client_test_pins` pins the lattice those counts are computed from, field for field, against the real pyramids.

Two properties make this a tier rather than a second opinion:

- **Grey does not move with it.** The coverage plane is record-derived (`crate::coverage::TileOverlay`), not read out of whichever pyramid answered, so the two tiers cannot disagree about where the radio looked.
- **The claim shrinks with the resolution.** `resolution.source` is computed exactly as it is on the fine tier: an overview tile folded from a coarser source cell reports `survey-overview` and `resolution.fold` says which axis replicated. A tier that answered cheaply still has to say what it is.

#### `axes.*.max_level` — how far up each axis can actually be **read** (T-482)

`levels` and `max_level` answer different questions and a client that conflates them addresses a node that is *named and cannot be built*: `levels` is how many levels the address lattice **names**, `max_level` is how far up it can actually be **read**. They differ whenever the store ladder is shallower than the address lattice — on the shipped geometry a **12 × 15** view lattice over a **4 × 4** store — and before this field the route declared the larger number and then `400`d a large part of the grid it had just declared. Measured against a real `hk serve`, an aggressive zoom-out made **117 tile requests of which 107 were refused**; the client was not misbehaving, it was obeying the only bound anyone stated. *Nothing said is never permissive*, inverted: **a client cannot clamp to a bound nobody declares.**

- **It is a BOX, and that is the strong reading.** Every address with `level_f ≤ frequency.max_level` **and** `level_t ≤ time.max_level` is servable; beyond it the route answers `400` because the store behind the lattice cannot back the tile. The box is walked address by address in `crates/hk-api/tests/tile_ceiling.rs` rather than asserted — a ceiling that still refuses is the same defect one notch down. **Whether an address is servable never depends on coverage (T-515, in T-507):** a tile whose plane is uniformly `"unobserved"` is answered from the coverage map without reading the store (T-461), but only at an address the read itself could serve — outside that set it gets the read's own `400`, so the same address cannot answer `200` over an unsampled band and `400` once the band is sampled.
- **It is computed from the open pyramid's geometry, against both of this route's work bounds** — the per-tile source-cell budget (`resolution.budget`) and `hk-store`'s fold budget — **on the worst store either could meet**. A tile that already exists costs no fold budget, so capture can only make a read cheaper; the number therefore does not move as the store fills, and it is a property of *this server's* geometry rather than a constant.
- **The servable set is an AREA constraint, not a per-axis one, so a box loses part of it.** Work goes as the tile's area (`tile_hz × tile_s`): a store level's grid over the tile is `tile_hz / f_cell` by `tile_s / t_cell`, so the real bound reads `level_f + level_t ≤ k` — an anti-diagonal. (T-480 verified the area nature independently by moving `cells`: `level_f = 10` is refused at `cells = 256` and served at `cells = 128`.) A box inscribed in an anti-diagonal cannot reach both far corners: on the shipped geometry `(9, 1)` and `(5, 5)` are both servable and both maximal, and no single box holds both. **What a box loses is the very-wide-*and*-very-tall pairs** — exactly the combination whose work bound is the reason for the ceiling. The pair served is the box whose corner tile covers the most area; the tie among those goes to frequency, measured rather than argued: covering the canvas's widest view (1 MHz–6 GHz by a retention window of tens of minutes) costs 32 tiles at `(9, 1)`, 30 at `(8, 2)` and `(7, 3)` and 118 at `(5, 5)`, so the *sum* is what matters and the tie-break barely does — and it goes to the axis the defect appeared on.
- **Readability, not existence.** A node a scheme does not *have* — a welded ladder's off-diagonal, already answered by its own `404` and reported by `axes.store_node` — is not counted against the ceiling. Conflating the two would collapse a ladder's ceiling to `(0, 0)` while saying nothing new.
- **It is stated for a 256-cell tile, this route's own unit — not for the answer's `cells`.** The bound *is* on area, so it moves with tile size; the *declaration* deliberately does not, because a client bootstraps its lattice from a cheap `cells=8` probe and then renders at 256 (`ui/src/surface/tile.ts`, `latticeOf(probe, RENDER_CELLS)`). A ceiling quoted per answer would be cached against tiles 32× wider on each axis and would be a lie for every one of them — measured in the browser tier, where the probe read back `(11, 9)` and 69 addresses inside that box were refused at 256. 256 is also the widest tile this route accepts, so the pair can never over-claim for a smaller one; a caller using `cells` below 256 can genuinely read further than this says, and gives that reach up for a number it can cache.
- **Absent, a client must assume `levels - 1`**, which is what `ui/src/surface/lattice.ts` does. That is the pre-T-482 behaviour and it is what the flood was.

#### The coverage plane: a table of **distinct** planes, run-length encoded (T-467)

`coverage` on this route is **not** `/api/coverage`'s per-cell form. It was, and measured against the demo backend that cost **99 % of a 19.34 MB tile body**: each of 65 536 cells serialised `{"state":…,"duty":…,"observed_s":…,"last_s":…,"spans":…,"center_hz":…,"sample_rate_hz":…}` at ~146 B, **twice** — once as `coverage.any` and once as `coverage.devices[0]`, byte-identical on a one-device server — to carry the one field a renderer reads. Measured in-process on the same 256 × 256 grid: **19 818 236 B → 2 906 B, a factor of 6 820**; end to end the tile body went **18.42 MB → 1.12 MB**.

- **`states` is the alphabet, served with the planes.** A code is never resolved against an alphabet the answer did not state. There are **four** and they never collapse: `unobserved` (nothing ever looked — grey, and *only* this is grey), `observed`, `unknown` (T-423: we no longer know whether we looked — not grey, not a level) and `excluded` (T-595: sampled, and **deliberately left out of analysis** — the receiver's own DC/LO notch. The measurement exists and must be drawn; only the detector skipped it). `excluded` was **appended**, so every code an older client cached keeps its meaning, and a client that does not know the word falls through to drawing the level — the safe direction, since the level is real.
- **`planes[i].runs` is a flat `[code, count, code, count, …]`** over the cells in `grid.order`. The counts sum to `planes[i].cells`; each code indexes `states`. A coverage plane is the rasterisation of tuned **spans**, so it changes state only where a band begins or ends — a handful of runs a row, not an entry a cell.
- **`planes[i].uniform`** is the one state the whole plane is in, or `null`. Derived from the same runs, never asserted beside them.
- **A plane appears once.** `any.plane` and each `devices[].plane` are indices into `planes`, so two front ends whose coverage genuinely differs cost two entries and a front end whose coverage *is* the union costs an index. The duplication this ticket was filed about is gone by construction, not by a special case for one-device servers. When devices genuinely differ the cost is one RLE plane per distinct plane — a few KB each, linear in *distinct* planes rather than in devices × cells.
- **`selected.plane`** is the index the route's own selection rule picks: `any` → the union; a named device → **that front end's plane and never the union**. `present: false` with `plane: null` is a named device this answer holds no plane for, which is `unobserved` *for that device* — a coverage answer, not a missing one.
- **No cell on this plane carries a measurement key of any kind**, so there is nothing here a client can read as a level of zero. That is *stronger* than the per-cell form's rule that an unobserved cell carries no measurement keys, not weaker: here no cell does. The measurement plane is `grid`, and it is separate on purpose.
- **The per-cell sampling metadata moved, it did not vanish.** `duty`, `observed_s`, `last_s`, `spans`, `center_hz` and `sample_rate_hz` are a question about *one* cell — hover — and [`GET /api/coverage`](#get-apicoverage--the-coverage-map-grey-means-genuinely-unobserved-t-368) answers it per cell over any `f_lo`/`f_hi`/`t0`/`t1`/`cells`/`rows`, in the same four-state vocabulary. `/api/timeline`'s overlay is unchanged and still serves the per-cell form.
- **A plane that does not decode exactly is not a coverage answer.** An odd run list, a code outside the alphabet, a run that overruns, a total that is not `cells`, or a missing `states`: the client throws and the place stays *pending*, never grey and never observed (`ui/src/surface/tile.ts`).

#### `shadow` — the last-known / stale tier, a band's most-recent-known value (T-519, T-527, [ADR-0020](adr/0020-last-known-shadow-tier.md))

A band swept and then departed is not grey: it was observed, and the newest thing known about it is a real measurement. `shadow` carries that value across the tile's rows so the client can draw it **dimly** (T-520) — observed at some point = shadow; never observed = grey; observed now = bright again. It is its own honesty tier, **last-known / stale**: a measurement *of `last_t_s`*, carried to a row it is not a measurement of.

- **Grey's meaning is unchanged, and it is still decided by `coverage` alone.** Draw a shadow **only** where the selected coverage plane says `unobserved`; a cell with no run over it stays grey — no retained measurement reaches it. `unknown` (T-423) and *observed-not-yet-measured* keep their own marks.
- **Runs, per column.** Run `i` covers rows `[row[i], row[i] + rows[i])` of column `f[i]` (the grid's axes, row 0 earliest). `last_db[i]` is a max-hold measured at `last_t_s[i]` (absolute capture time), resolved at `sources[src[i]]`'s cells. The parallel arrays — `row`, `rows`, `last_db`, `last_t_s`, `src`, `fill` — all have `runs` entries; runs never overlap.
- **EVERY time gap in a column that was ever observed is filled, and `fill[i]` says which way it was read (T-527).** `fill[i]` is a code into `fills` (`["forward", "backward"]`), the same shape as `coverage.states`:
  - **`forward`** — the nearest **past** sample, carried down. `last_t_s[i]` is when it was **last** seen and lies at or before the run. *This is what it looked like when we last saw it.*
  - **`backward`** — the column's **first-ever** sample, carried **up** into the stretch before it. `last_t_s[i]` is when it was **first** seen and lies **after** the run. *This is what it looked like when we first saw it.* This is the **only** value in this plane read backward in time: it exists only above a column's first sample, only where the search found nothing older (a column with a before-tile value has its head carried forward instead), and it always comes from `this-tile`. "First-ever" is as strong as the search was: where `search.unsearched` is non-empty, something older may exist in a window that was not read, so a backward run there means *the first sample we have*, and the run still says which way it was read.
  - Either way `last_t_s[i]` is the boundary **nearest** the run, so `|row time − last_t_s[i]|` is the smallest age the evidence supports; the sign of that difference is the direction, and `fill` states it so no client has to infer it. `backward_runs` is how many of the `runs` are backward — 0 on a tile with no arriving column, which is every tile the coverage short-circuit answers.
  - A column with **no sample and nothing older carries no run at all**: it was never observed, and grey is the right answer for it. Before T-527 a column first seen part-way down the view was greyed above its samples although it had been observed, which is the claim this fixes.
- **A shadow never replaces a measurement.** No run covers a row where `grid` holds a value. Down each column the carried value starts as the newest one **before the tile** (`sources[].from = "before-tile"`) and is replaced by the tile's own value at every row it measures (`"this-tile"`), so a band seen part-way down the tile and then departed carries the value it was last seen with.
- **Nothing past the data edge.** Rows at or after `edge_s` (the store's newest frame) carry nothing: a shadow never paints the future.
- **Where the before-tile value comes from.** A query-time search over the **spectrum-history** pyramid (scheme 1, whose ladder runs seconds → days; a server without one searches the tile's own store at its level 0): newest-first and fine-to-coarse, each stage reading one level over the part of the past the finer stage did not, so the whole retained horizon costs a few hundred rows per column. It stops when every column has a value or nothing older is held. Nothing is maintained for it and capture pays nothing (T-453). `search.stages` lists what was read.
- **The search is never carried backward, and what was not searched is said.** (The one backward read is the `backward` fill above, which is this tile's own first sample and never a search result.) A coarse cell straddling the tile's start is used for a column only where this tile's own grid proves it held nothing for that column before the cell ended. A window over budget, or whose coarse cell is not folded yet, is listed in `search.unsearched` — a column with no run is unobserved in `[searched_from_s, before_s)` **outside** those windows, and nothing is claimed about earlier. Frequency resolution coarsens with age because the ladder is welded; `sources[].f_cell_hz` states it, and a coarser source replicates (T-334's direction).
- **Cost, and the budget's units (T-523).** The whole search reads at most `search.max_source_cells` = **`cells` × 512 rows** — the **tile's** scale, not the store's, because the shadow decorates a `cells × cells` grid with at most `cells` column values. Rows rather than an area, so a probe-sized tile reaches as far back as a full one; the ladder is fine-to-coarse, so 512 rows spread over it reach days. That total is a quarter of what `resolution.budget.max_source_cells_per_lock` allows a *single* hold, so no hold this search takes can come near lengthening one (it still takes one per stage/slice, reported as `search.chunks`). It shipped (T-519) with the tile read's own two bounds, which made T-461's coverage short-circuit — the route's *cheapest* answer, where the shadow also runs — one of its most expensive: ~84 ms on an 819 MHz tile on the demo backend, against ~3 ms for the same address before the shadow existed. In a debug-profile test over the departed-band fixture, the same cut takes the 819.2 MHz tile's search from 117.97 ms to 10.70 ms and the worst address of a ten-level zoom burst from 225.18 ms to 13.28 ms, with the same runs per level. **The budget is paid in resolution, never in reach**: a stage the search cannot afford is skipped and the next *coarser* level covers that window, which is why every run states the level that answered it (`sources[].level`, `f_cell_hz`, `t_cell_s`). Over spectrum no tile holds, the search answers from the pyramid's tile index without reading a cell. Measured in `hk-store`'s `last_known_cost_is_bounded_on_a_tuned_and_a_device_wide_viewport` and, per zoom-burst address, in `hk-api`'s `tests/tile_cost.rs`.
- **The backward fill costs no search (T-527).** It reads no source cell and changes no query: it is one push inside the row walk the carry already makes over the tile's **own** cells, and on the coverage short-circuit — which passes no grid — it cannot fire at all, so T-523's budgeted path is untouched. Measured over the same zoom-burst addresses, paired before and after on one machine: on the short-circuit, identical `source_cells` and `chunks` per level and the worst search 13.76 → 13.10 ms (noise); on the full path over an *arriving* band, per-level search 3.42 → 2.29, 8.48 → 7.85, 14.78 → 14.92, 12.26 → 12.22 ms — no systematic change — while level 0 turns **32 768 grey cells into shadow** and levels 0–4 go from *no runs at all* to one per column. `the_backward_fill_is_measured_and_its_case_counted_across_a_zoom_burst` is that measurement. **Its case is not universal and is not claimed to be:** a head gap needs a column whose record *begins* inside the viewed window — a capture's start, or a retune onto new spectrum — and a band tuned for the whole window has none.
- **Size.** Runs, not a plane: a departed band is one run per column (~5 KB for 64 columns including the search block), a fully measured tile carries none. `fill` adds ~2 bytes per run (939 B on a 256-run block, +0.1 % of a measured tile's body and +8 % of a short-circuited one's). The shadow is **not device-scoped** — the pyramid is not, and this carries its values.
- **Served on both paths.** A tile the coverage map answers alone (T-461, below) is exactly where a departed band's shadow lives, so `shadow` is served there too, searched against no grid.

**The view lattice's floor is the store's, not §6.2's.** §6.2 put node (0, 0) at 100 kHz × **128 s** against a 120 s IQ retention, which puts the entire live view inside one time cell and pins `level_t` at 0 for every realistic pane — the de-welding buying nothing on the axis it exists for (T-437 finding **F1**). Anchoring at the open pyramid's level-0 cell is that fix.

**The view lattice's floor is the display stream's own bin and row (T-484).** The open pyramid's level-0 cell is now `sample_rate / spectrum_fft_len` × the display row period — 2343.75 Hz × 40.1 ms at 2.4 Msps — and the frames folded into it are the rows [`GET /api/stream/spectrum/live`](#streams) publishes. So at node `(0, 0)` the fold is **1:1**: one FFT bin of one published row per cell, `grid.max_db` is the number that row carried, and `resolution.fold` reads `exact` on both axes. T-483 measured what the previous floor cost, on one capture read twice: a cell stood in for **66.5** display measurements, the quiet band's noise floor read **+10.5 dB** high (a max-hold over ~10³ FFT cells lifts noise and leaves a peak where it is), contrast fell 4.5–5.0 dB on narrow emissions, and a one-second cell kept **3 %** of a broadcast station's level variation. All four are now 1.0 cells, +0.1 dB, 0.1 dB and 100 %.

Two consequences a client can see. **The lattice stays 4 × 4, and zooming out costs more tiles for it.** The work budget bounds `level_f + level_t` by *level index* while a viewport's demand is set by the floor's absolute cell size, so a 25× finer time cell spends ~4.6 levels of reach; since [`axes.*.max_level`](#axesmax_level--how-far-up-each-axis-can-actually-be-read-t-482) a client **clamps** rather than being refused, the cost is fan-out, not a `400`. Measured on a 1600 × 800 pane at the ceiling `(9, 1)`: the tuned window 8 tiles against 2, a 20 MHz × 10 min sweep 150 against 21, the whole device × 10 min 600 against 24 — bounded in practice because across 6 GHz nearly every tile is unobserved and answered from the coverage map. A deeper time axis would repair it (4 × 6 costs 8 / 20 / 316) but is blocked on `servable` admitting a level it cannot build; see `hk_pipeline::history::VIEW_T_LEVELS`. And **a cell's own tier is readable from `resolution`**: `answered.level` `0` with `fold.*.direction` `exact` is a published row, and anything coarser is a fold of those, declared per axis.

#### The measurement plane is served as binary16 on request (T-533)

`grid.max_db` is 65 536 JSON decimal numbers on a rendered tile — **1 197 118 B of a 1 878 289 B live body, 64 %** — and its destination in the one client that reads it is an **R16F texture**: seventeen significant digits sent, eleven bits kept. `?planes=f16` spells that plane as base64 of little-endian IEEE 754 binary16 instead.

| | `?planes=json` (default) | `?planes=f16` |
|---|---|---|
| `grid.max_db` | the array | **absent** |
| `grid.planes.max_db` | **absent** | `{type, byte_order, transfer, cells, bytes, scale, absent, data}` |
| `occupancy_max`, `coverage`, `frames` | arrays | arrays, unchanged |
| `grid.encoding.planes` | `"json"` | `"f16"` |

- **Same values, one spelling.** The packed plane is the same cells in the same row-major order, equal to within binary16's own precision — which is the precision the texture keeps either way. `NaN` is what `null` is: **not observed**, never a level of zero (C26). `crates/hk-cli/tests/api_contract.rs` asserts the two cell for cell on one live tile.
- **The wire states its own type**, and a reader that does not recognise `encoding.planes` must **refuse the tile** rather than decode it as the spelling it does know — a plane read against the wrong type is a measurement invented, not a degraded one. `ui/src/surface/tile.ts` throws `TileDecodeError`, which leaves the place *pending*, never grey. A different packing in future gets a **new name**, never a redefinition of `f16`. An unrecognised `planes=` value is a `400` naming it and the accepted set, never a quiet fall back to the other spelling.
- **Only `max_db`, because only `max_db` wins.** For the other three planes JSON is the *smaller* spelling: measured on the same tile, `frames` is 131 073 B as text (two distinct values over 65 536 cells) against 349 528 B as base64 `u32`, and `occupancy_max`/`coverage` lose likewise. A "pack everything" mode would have grown three planes by 394 kB to shrink one.
- **The absent one is ABSENT, not empty or null**, in both directions — an empty array would read as a grid of no cells, which is a different claim from a grid whose cells are spelled elsewhere. The uniform short-circuit grid (below) carries no plane in either spelling and still states its `encoding`.
- **Measured through the route, one address, four spellings back to back** on a 256 × 256 tile. Originally (2026-09-20, a fully-populated live tile): **1 879 209 B** as JSON, **856 178 B** packed, **244 012 B** JSON gzipped, **117 382 B** packed *and* gzipped — **16×**. **Re-measured on relanding** (T-700, 2026-09-22, after T-571 changed how a tile is produced and T-595 added `excluded`; the acceptance fixture's tile, which carries more absence and therefore compresses further): **1 202 453 B** → **912 435 B** packed → **71 005 B** JSON gzipped → **43 090 B** packed *and* gzipped, **27.9×**. The magnitudes move with the tile; the *ordering* is what the contract test asserts, because both levers pay and neither subsumes the other — JSON decimal text is high-entropy by construction, so compressing it is not the same as not sending it.
- **Where the win lands, stated honestly.** `cost.build_ms` did not move (20.0 → 20.4 ms originally; 16.7 → 17.8 ms on the reland, inside the run-to-run spread): the route's own work was never the float formatting. Measured in Chrome over **loopback**, one tile's fetch-to-decoded hop is 36 → 31 ms and a 16-tile pane row at the four-slot cap is 182 → 166 ms — both dominated by tile *production*, not by the bytes. The body is what a tunnel, a phone or a second machine waits for, and what the browser parses. Making a tile produce faster is a different ticket from making it smaller, and this is the second.

#### `Accept-Encoding: gzip` (T-533)

Every routed JSON response is gzipped when the request asks for it and the body is at least 4096 bytes; the answer then carries `Content-Encoding: gzip`. **`Vary: Origin, Accept-Encoding` is stated on every answer, in ONE header** (T-700) — both on the compressed one and on the identity one, since either form may be the one a cache stores; two separate `Vary:` lines is the bug this shape exists to prevent, because a cache reading only the first keys on `Origin` alone and can then serve the gzipped body to a client that refused gzip. [`/api/tiles`](#get-apitiles--one-tile-of-the-unified-surface-at-independent-level_f-level_t-t-438-docs16-7-step-5--8) answers itself rather than through that tail (T-574 gives a *sealed* tile an `ETag` and `Cache-Control: immutable`), so it applies the coding itself, over the same bytes — and its **`ETag` is computed over the uncompressed JSON**, since gzip is a transfer coding and the representation a cache validates is the same either way. A `304` carries no body and so no coding. **It is a transfer coding and never a representation**: the bytes a client decodes are identical to what it would have received without the header, which is what the contract test asserts (inflate, then compare the JSON). `gzip;q=0` is a caller saying it *cannot* read gzip and is honoured. The threshold exists because below it a gzip member's own header and trailer are a large share of what is sent; a live `/api/tiles` body is two orders of magnitude above it and compresses by an order of magnitude or more (re-measured on relanding, T-700: a 256-cell tile 1 063 213 B → 20 280 B, and the acceptance fixture's 1 202 453 B → 71 005 B; the factor moves with how much of the tile is absence).

#### The budget is a fold target, never a level selector (T-437 finding F2)

T-437 measured the defect on [`/api/history`](#get-apihistory--region-over-time-grid-t-017-aware-042): same window, same band, only `max_f` changed — `max_f=384` served 38 784/38 784 cells observed, `max_f=256` served 384/576 (**67 %**). Tightening the *frequency* budget 1.5× cost **34× of time resolution and greyed a third of the window**, because `max_f` picks a *level*. That is a grey-honesty violation caused by level choice, which [docs/16](16-coverage-tile-pyramid-and-full-spectrum-view.md) §4 does not name: §4 guards the fold, and the fold is fine — here a cell reads *unobserved* while level 0 holds the measurement.

This route cannot express that bug, for four reasons, in order of how much they rest on judgement:

1. **There is no caller-supplied per-axis cell budget at all.** The tile's grid is always exactly `cells × cells` laid on the tile's own extent; the *address is* the budget. So `level_f` and `level_t` are structurally independent — changing one cannot move the other's cell size by a rounding, which is exactly what F2 did.
2. **The level is chosen finest-affordable-first**, never coarsest-adequate. Folding a finer level onto the tile's grid can never grey a cell that level holds: the fold is a max and a sum, so an output cell is observed if *any* source cell inside it was. Only the other direction — a source cell **coarser** than the tile's own, which repeats one measured value across output cells — is a claim, and `resolution.fold.<axis>.direction` states it per axis (`exact` / `folded` / `replicated`) and downgrades `source` to `survey-overview`.
3. **Candidates are ordered by cell *area*, explicitly, never by level index.** T-434's warning: index order is a coarseness order only for a ladder — in a lattice node (1, 0) outranks (0, 3) in index while being *finer* in time. Area is a total order that agrees with the partial coarsening order, so it can never put a coarser level first.
   **A candidate is affordable to read *and* to build (T-494).** A level is listed only if its grid over the tile fits the read work budget **and** folding it from nothing, per read chunk, fits the store's materialize budget, charged for the worst chunk start on the read's own start grid. A level that cannot be folded is never a candidate, so the walk in (4) cannot reach a fold refusal. Before T-494 such levels were listed. On any store deeper than 4 × 4 they made the route 400 inside its own declared `max_level` box.
4. **When a level holds nothing the read walks coarser** through the remaining candidates (T-426's rule, in the direction this route's preference makes meaningful: the byte budget evicts the finest tiles first, §5.5). `resolution.answered.level` is the tier that **actually answered**, `resolution.candidates` every affordable level and `resolution.tried` every one consulted — a silent fallback would trade one lie for another.

`resolution.budget` carries the two work bounds so nothing has to be inferred from the grid: `max_source_cells_per_lock` bounds one history lock hold, `max_source_cells_per_tile` bounds a whole request. **Both bound work, not resolution.**

#### Cost, and the two caps

T-437 measured rendering at p95 2.2 ms for 48 panes and tile **production** at ~500 ms per tile — three orders of magnitude apart — so production is what this route is designed against. Measured server-side through the real store and the real fold: **11.6 ms** for a 256 × 256 tile, 1.4 ms at 64 × 64. A 208-tile screen is therefore ~2.4 s of production, which is a **prefetch-order and precompute** problem, not a rendering one.

- **`cost.chunks` is the number of history lock holds this tile took.** The read is chunked into whole output rows, re-acquiring the lock per chunk, so a tile fan-out at the live edge can never lock ingest out for a whole tile — the report builder's ≤ 256-row discipline, applied to §5.5's cap (3).
- **`cost.in_flight_limit` is server backpressure**, chosen against `hk-store`'s lock behaviour rather than a browser's connection limit: the history store is behind one mutex, so concurrent tile reads serialise on it anyway and a deeper queue only lengthens the stretch during which ingest competes for it. Over the cap the answer is `503` **naming the cap**, which is what lets a client cancel tiles for a viewport it has left (§5.5's cap (1) is LIFO with viewport cancellation) instead of waiting.
- **`cost.in_flight_share` is *this client's* cap, and it is the number a client should operate at** (T-630). The server-wide cap is unchanged; what changed is whose request meets it. Measured before this existed: while one tab enumerated a wide viewport it held **all four** slots continuously — it re-asks the instant one frees — so a second tab's *first* request, the one it cannot start without, competed on equal terms with the thousandth request of a tab that is already drawn. The second tab booted in 8.2 s and 11.7 s after 7 refusals, and twice did not boot at all. Raising the cap would only move that failure and would spend capacity the capture thread pays for (T-453), so the cap stays and the **policy** is two rules:
  - **A share of the cap per client**: `in_flight_share = ceil(in_flight_limit / clients)`, so two clients get two slots each and a third is guaranteed one. A client over its share is refused even when the route has a free slot, which is what makes the slots a newcomer needs appear without anyone yielding them politely. `cost.clients` is how many are asking, so a client can see why its share moved, and `cost.in_flight_held` is what it holds.
  - **Priority by what the request is**: a client that has never been *served* a tile is bootstrapping — asking for first paint, not fill — and while one exists the already-drawn clients are admitted only up to `in_flight_limit - 1` (`cost.reserved: 1`). This is T-457's visible-fetch precedence and T-459's "no visible fetch is starved" at the one place where the competing fetches belong to different clients. It costs nothing when nobody is bootstrapping: it is armed by the newcomer's own first (refused) request and disarmed by its first success.
- **`client` is a declared identity, and a share it does not renew is reclaimed.** A tab's tile reads go over a pool of connections, so a connection is not a client; every tab of one browser carries the same token, so a session is not one either. `client` is therefore an opaque id the page makes for itself, fresh per page load (a reload is a newcomer, and its predecessor's abandoned reads are not charged to it). It is `[A-Za-z0-9-_.:]{1,64}`; anything else, or none, shares one **anonymous** bucket that behaves exactly as the route did before T-630 — so `curl` and the CLI are unaffected, and a caller that wants a share of its own says who it is. Since nothing tells the server when a client goes away, the table is a cache of *who is asking now*: an entry holding no slots and silent for 10 s is forgotten and its share returns to the clients still here (a leaked share is the failure T-454 paid for once with slots). Slots cannot leak either way — a slot releases both counters when the read ends, however it ends. At most 64 identities are tracked; past that the coldest idle one is dropped, and if every tracked client is busy a new identity is served from the anonymous bucket rather than growing the table.
- **`cost.fair_share: false`** means the share is switched off (`HK_TILE_FAIR_SHARE=off`) and the route is first-come-first-served. Nothing in the product sets it: it exists so `ui/e2e/surface-contention.e2e.mjs` has the pre-T-630 route to go **red** against.

#### What a tile does not carry

- **No emitters** (§5.3). Identity gating is per-caller and a tile is not; a sealed tile is immutable and an emitter set never is. The highlight layer is [`/api/tiles/events`](#get-apitilesevents--the-coarse-zoom-event-aggregate-t-438-docs16-53) below.
- **No percentiles.** De-welding costs them (T-434): a tile keeps one histogram per frequency cell over the whole tile, which is the parent cell's histogram *only* when a child tile is exactly one parent time cell — the weld. `grid.percentiles` says `unknown` rather than approximating a distribution; the noise floor stays a scheme-1 question, asked through [`/api/history`](#get-apihistory--region-over-time-grid-t-017-aware-042).
- **Never `live-iq`** — an **under-claim** since T-484, kept deliberately. T-439 made the view lattice's finest node the growing edge (*"live" is a viewport, not a mode*, [docs/16](16-coverage-tile-pyramid-and-full-spectrum-view.md) §8.1) and T-484 made its cell the display FFT's own bin and row, so a node `(0, 0)` tile now *is* “live IQ from the front end at the resolution shown” and the stronger claim would be true. This route still answers `"spectrum-history"` for it, because `source` is defined by [`live_window_verdict`](#live-iq-detail-versus-overview-t-341) as a claim about **span**, shared with three other routes, and re-pointing it at resolution is a contract change no reader has asked for. The rule the table states applies: under-claiming costs a styling cue, over-claiming is the lie. A client that wants the distinction reads `resolution.answered.level` and `resolution.fold.<axis>.direction` — level `0` with `exact` on both axes is one published row per cell.
- **No cell for the newest moment.** At a growing edge the newest cells are routinely **observed but not yet measured**: `coverage` says the front end was tuned there and sampling, and `grid` has nothing for them yet, because the fold is behind capture by at least one history frame. That is the normal state of a live edge, not an error — and it is **neither grey nor `"unknown"`**: grey is *nothing ever looked*, `"unknown"` (T-423) is *we no longer know whether we looked*, and this is *we are looking right now*. Three states, three marks (T-441).

#### An unobserved tile is answered from the coverage map (T-461)

A tile's default is **no data**, and a tile over spectrum no record says was ever sampled ran the identical `O(cells²)` production path as an observed one. Measured: **92 ms** of `cost.build_ms` and **2 561 726 B** on the wire to say *nothing here*, with `source_cells: 65 536` — the whole grid walked. And that is the **common** case, not an edge one: T-437 measured the default full-device view at **99.4 % grey** before history accumulates, settling to 55.2 %. The view that opens first pays the most.

So when the **selected** coverage plane — the one `coverage.selected.plane` names, which is the same plane this answer serves and the same one the renderer greys from — is `unobserved` for every cell of the tile's extent, the route answers from it:

```jsonc
"grid": { "nt": 256, "nf": 256, "t0_s": …, "t_cell_s": …, "f_lo_hz": …, "f_cell_hz": …,
          "uniform": { "max_db": null, "occupancy_max": null, "coverage": 0.0, "frames": 0,
                       "observed": false, "rule": "…" },
          "cells": 65536, "observed_cells": 0, "range_db": null, "unit": "dbfs",
          "percentiles": "…", "semantics": {"…": "…"} },
"resolution": { "answered": null, "candidates": [], "tried": [],
                "short_circuit": { "applied": true, "selected_plane_uniform": "unobserved",
                                   "rule": "…" }, "…": "…" },
"cost": { "build_ms": 3.1, "source_cells": 0, "chunks": 0, "…": "…" }
```

- **`grid.uniform` replaces the four per-cell arrays, which are then *absent*** — not empty, because an empty array reads as a grid of *no cells*, which is a different claim from a grid of cells that hold nothing. `max_db: null` is the **absence of a level, never a level of zero**, and `observed: false` says so in a second way. `nt`, `nf` and `cells` are unchanged, so the grid is still the tile's own `cells × cells`.
- **It answers "unobserved", which is neither "quiet" nor "zero".** Grey is still decided by `coverage` and by nothing else; this grid says only that the pyramid holds nothing for any cell.
- **It fails closed.** `short_circuit.applied` is true **if and only if** `short_circuit.selected_plane_uniform` is `"unobserved"`. A partially observed tile takes the full read; a uniformly **`"unknown"`** tile (T-423 — a row wholly before the record horizon, where no surviving record can say either way) takes the full read, because `unknown` is not `unobserved` and the pyramid may well hold measurements there. `short_circuit` is served on **both** paths, so `applied: false` names the reason.
- **The predicate cannot be weakened by the grid it was evaluated on.** The coverage plane is rasterised at this tile's own cells or coarser (the per-grid cap in `hk_store::coverage::grid_over`), and a coarser cell is `unobserved` only when *no* tune span touches it at all — so uniform-unobserved at a coarser grid implies uniform-unobserved at a finer one.
- **It is a cheaper spelling of the full path's answer, not a second answer.** The constants in `uniform` are exactly what a real store read produces over never-sampled spectrum (asserted against one in `hk-api`'s tests), and the client's own rule is that *a cell the coverage plane calls unobserved stays unobserved even with a level beside it* — so the measurement the full read would have produced is discarded by the renderer cell for cell either way.
- **`cost.chunks: 0`** is literal for the grid: no history lock was taken for it. The last-known search behind `shadow` (T-519) is separate and states its own holds in `shadow.search.chunks`; over spectrum no tile holds, it answers from the pyramid's tile index without reading a cell.

Measured on the same fixture, before and after (`crates/hk-api/tests/tile_cost.rs`, a test-profile binary; `body_bytes` is the uncompressed bytes the HTTP layer would write): **2 561 726 B → 7 568 B** (338×), `cost.build_ms` **92.1 ms → 3.1 ms** (29×), `source_cells` **65 536 → 0**. The body is a *constant*: 7 549 B at 64 × 64 and 7 563 B at 256 × 256 — sixteen times the cells for fourteen more bytes. The remaining ~3 ms is the coverage rasterisation itself, which is the answer rather than overhead: reading the same plane the renderer greys from is the point.

#### The hot-tile cache (T-572)

A viewport that has not moved re-reads the same tiles every poll, so a bounded in-memory LRU sits in front of this route.

- **Sealed tiles only, and that is the whole correctness argument.** A sealed tile's own time extent has fully passed the pyramid's watermark, so a frame landing inside it is by definition late and dropped: it can never change again (the same fact that earns it an ETag and `immutable`, T-574). A **live** tile at the growing edge changes on every arriving row and is never looked up, never inserted and always re-read — a stale live tile breaks *"rows append in real time"* exactly as badly as a missing one. The distinction is structural, in the insert path, never a timer.
- **`cost.served_from: "hot-tile-cache"`** appears on a cached answer and is absent otherwise. It is a diagnostic of the READ, not of the tile: it is removed before the body is hashed into an ETag, so a hit and a miss validate identically and a re-read is still the 304 T-574 promises.
- **Bounded in bytes AND in entries** — 32 MiB / 256 entries, whichever binds first — so residency does not grow with node count (T-453). Least-recently-*used*, not least-recently-inserted. A body larger than the whole cache is never held.
- **The coverage plane beside a sealed grid can still move**, being derived from the observation log, so every entry is dropped whenever that log's `written` or `segments_deleted` counters move. The cache reads those two atomics and writes nothing: the capture thread is not on this path at all.
- **Counters are on `GET /api/status` as `tile_cache`** (`entries`, `bytes`, `max_entries`, `max_bytes`, `hits`, `misses`, `evictions`, `invalidations`) — never in a tile body, where they would change on every read and with them the ETag.

### `GET /api/tiles/batch` — a viewport's worth of tile addresses in one request (T-573)

A viewport needs tens of tiles and used to ask for them one HTTP request at a time. This route takes the addresses together and answers them together. **It is a transport change, never an analysis one:** each entry's `tile` is byte-identical to what `GET /api/tiles` answers for that address alone — the same `key`, the same independent `(level_f, level_t)` pair, the same `coverage` plane, the same `resolution` block, the same per-tile `cost`.

Query: `device`, `scheme`, `cells`, `planes` and `client` are shared by the batch (they are properties of the viewport, and one request is one asker); `addresses` carries the per-tile part.

```
GET /api/tiles/batch?addresses=<level_f>.<level_t>.<f_index>.<t_index>[,…]&cells=256&planes=f16
{ "requested": 5, "returned": 5, "truncated": false, "remaining": [],
  "limits": { "max_addresses": 64, "max_response_bytes": 8388608,
              "over_addresses": "refused (400), naming the cap",
              "over_bytes": "truncated, with every unanswered address listed in `remaining`" },
  "tiles": [ { "address": { "level_f": 0, "level_t": 0, "f_index": 12, "t_index": 3,
                            "spelling": "0.0.12.3" },
               "status": 200, "tile": { "key": "…", "grid": "…", "coverage": "…", "cost": "…" } },
             { "address": { "…": "…" }, "status": 503, "error": "too many tile reads in flight …" } ],
  "statement": "…" }
```

- **A partial answer is expressible, and that is the point.** A viewport where three tiles have data, one is genuinely unobserved and one was refused is ONE response carrying three 200s, a 200 whose own `coverage` plane says `unobserved`, and a 503. There is no status for the set beyond the transport's own 200, because collapsing a missing tile into an empty one is exactly the defect the coverage map exists to prevent. The three marks the canvas depends on — data, genuinely-unobserved, observed-but-not-yet-measured — are per address, where they already were.
- **The coverage short-circuit is untouched.** Each address goes through the same code path as `GET /api/tiles`, so a uniformly-unobserved tile is still answered from the coverage map with `cost.source_cells: 0` and `chunks: 0`, never reaching the generation path. A batch endpoint that made empty tiles expensive again would be a regression, not a win.
- **The in-flight cap is per address, still — and a batch is answered as wide as that cap allows, not one address at a time.** One producer slot is taken and released per address, under the named `client`'s share (T-630), exactly as a single-tile read takes it. The route answers a batch's addresses concurrently on up to `cost.in_flight_limit` (4) of them at once, so a batch costs its slowest member rather than the sum of all of them: answered one at a time, a live-edge tile waited behind every cold tile in the same batch, and a following pane's newest rows were still flat 8 s after first draw (`ui/e2e/live-edge.e2e.mjs`). The share still bounds the width. A worker whose address is refused while another worker is still running hands the address back and stops, so a batch narrows to the slots its client actually has instead of refusing its own members. Only when no worker in the batch holds a slot is an address answered 503, and then the caller re-asks for exactly those.
- **Two caps, two different answers, because they have two different causes.** `max_addresses` (64) is the caller's own doing, so it is **refused** with a 400 naming the cap — the caller knows precisely what still needs asking for. `max_response_bytes` (8 MiB) is a property of the grid rather than of the request, so it **truncates**: `truncated: true` and every unanswered address listed in `remaining`, in the spelling it was asked in, so the follow-up is a copy and not a re-derivation. At least one tile is always returned, so a single oversized address is never unfetchable.
- **A malformed address refuses the whole request**, rather than being skipped. A silently-dropped address is a tile the canvas leaves pending forever with nothing saying why.
- **No ETag, no immutable cache.** A batch is not a single representation and a mix of sealed and live tiles has no single validator; `GET /api/tiles` is where a sealed tile earns its ETag (T-574). The batch answer does honour `Accept-Encoding: gzip` like every other routed JSON answer (T-700).

### `GET /api/tiles/events` — the coarse-zoom event aggregate (T-438, [docs/16](16-coverage-tile-pyramid-and-full-spectrum-view.md) §5.3)

The same address as `/api/tiles` (plus the `/api/inventory` `state` filter), answering counts instead of spectra:

```jsonc
{ "key": { "…": "…" }, "extent": { "…": "…" },
  "counts": [0, 0, 3, "…"], "total": 12, "placed": 11,
  "emitters_scanned": 7, "emitters_truncated": false,
  "rule": "ONE COUNT PER EVENT, placed at the cell holding its START. …",
  "not_a_tile_channel": "counts change with every append to the observation ledger, …" }
```

`/api/events` caps at 500 expanded emitters and pages events, so a zoom-0 box of 6 GHz × 30 days hits that cap on every pan — §5.3 named the aggregate form and filed it as new work. It is **not** a tile channel: counts change with every append to the observation ledger, so sealing them into a tile would spend the immutability the tile storage was bought for.

**One count per event, placed at the cell holding its start.** An event is a presence interval with its own extent (CLAUDE.md invariant 1). Counting it once per row it crosses would inflate a long emission into a busy band, and scaling a sub-cell burst up to be visible would fabricate a timespan — the precise thing `/api/events` refuses when it computes `duration_s` itself. An event that began before this tile, or whose centre lies off its band, is counted in `total` and **not placed**; `placed` is what the grid holds, and clamping it to an edge would put activity in a cell it never occupied. An interval with no measured centre has no column and is likewise counted, not placed.

`counts` is row-major on **exactly** the tile's axes (`nt` time rows × `nf` frequency cells, earliest row and lowest frequency first), so a client indexes it with the tile's own index.

### `GET /api/status` — pipeline counters (T-027)

Opaque, per-build JSON object of counters (source samples, chain stats, control-loop stats under `"control"`, listen/chain admission under `"listen"`/`"budget"` when the pipeline exposes them, …), plus one field this route itself adds: **`t`**, the server's own wall clock (`Timestamp::now`, not the run's sample clock) at the instant the response was built — bare name, Unix seconds, per the units convention (T-351). Without it a caller could not tell a fresh read from a cached one, or measure its own clock skew against this device. Never content, never an identity. `404` when this server has no pipeline status function attached (e.g. a bare bridge with no composed pipeline).

**Compute providers (T-056, ADR-0007)** are reported under `"compute"`. They are chosen once per run and never change mid-run.

| Field | Type | Meaning |
|---|---|---|
| `compute.options` | object | Options in force after `--compute` and the `HK_COMPUTE*` environment: `provider` (`auto`, `cpu`, `cpu-mt`, `accelerate`, `gpu` or `cuda`), optional `stft`/`pfb` overrides, `threads`, `gpu_in_flight` |
| `compute.stft.<reader>` | object | For each always-on reader (`detect`, `history`, `spectrum`): `requested`, `provider` (the one used), `backend` (e.g. `cpu-rustfft`), `fft_len`, and `fallback` (why the requested provider was refused, or `null`) |
| `compute.providers[]` | array | Each provider's `name`, `compiled`, `conformant`, `usable` (`null` when not probed) and `detail` (a description, or why the provider is unusable) |
| `compute.stft_builds` | number | STFTs built this run (one per reader per segment, plus display rebuilds) |
| `compute.provider_changes` | number | Builds whose provider differed from that reader's first. Always `0` in a correct run |

**Baselines and attention (T-119, T-132, ADR-0012 §3)** are reported under `"attention"`: counters `folds`, `novel_folds`, `change_points`, `publishes`, `baseline_writes`, `errors`, plus the baseline memory bound. Loaded baselines are capped at 256 MiB by default (`HK_BASELINE_MEMORY_MB`, `0` = unbounded).

| Field | Type | Meaning |
|---|---|---|
| `attention.memory_bytes` | number | Gauge: approximate heap bytes of the loaded baselines |
| `attention.unloaded_engines` | number | Baseline keys saved and unloaded by the cap (reloaded on their next fold) |
| `attention.refused_folds` | number | Folds whose learning the cap refused; their novelty is still scored against what is loaded |
| `attention.gain_overflow_folds` | number | Folds under a gain state beyond a subject's kept gain slots (4 per level class); scored, not learned |

## Control API (T-050)

Device, display, recording and bookmark endpoints, all behind the bearer token, all audited once authenticated. **Six of them reach the radio and the rest do not** — see [Device actions](#device-actions-t-343) — and one more *commissions* retunes without performing any: [the in-app survey sweep](#the-in-app-survey-sweep-and-who-wins-when-it-and-the-user-both-want-the-radio-t-452). Every mutating body is a JSON object (`Content-Type: application/json`); an unknown field is `400 invalid`. Device endpoints (`center`, `rate`, `window`, `gains`, `bias_tee`) act on a *live* source ([`ApiState::live_controls`]); on a replayed recording they answer `409 not_live`, while display, recording and bookmarks keep working. Each of them also takes an optional **device selector**, `"device_id"` — see [Which radio](#which-radio-the-device-selector-t-511); omitting it is correct whenever the run holds exactly one front end, which is every run today.

| Method | Path | Body | Response | Notable errors |
|---|---|---|---|---|
| GET | `/api/control/state` | – | `{live, device, tuning, devices, run, scan, display_limits, transmit: {available: false, reason}, audit, routes}` — `devices` (T-511) is every live front end, `[]` on a replay; `device`/`tuning` are the singular default and are `null` when the run holds more than one | – |
| POST | `/api/control/center` | `{"center_hz", "device_id"?}` | `{tuning, run, device}` **(device action)** | 400 invalid/out_of_range/device_required, 404 unknown_device, 409 not_live/conflict/device_busy/finished |
| POST | `/api/control/rate` | `{"sample_rate_hz", "device_id"?}` | `{tuning, run, device}` **(device action)** | as above |
| POST | `/api/control/window` (T-529) | `{"center_hz", "sample_rate_hz", "device_id"?}` — the pair is **both required** | `{tuning, run, device}` **(device action, `action: "window"`)**. One whole capture configuration, one class derivation, at most one re-plumb | as above |
| POST | `/api/control/gains` | `{"gains": {"<stage>": <dB>, …}, "device_id"?}` | `{tuning, device}` **(device action)** (gains quantised per stage) | 400 invalid/device_required, 404 unknown_device, 409 not_live/device_busy |
| POST | `/api/control/bias_tee` | `{"enabled", "device_id"?}` | `{tuning, device}` **(device action)** | 501 unsupported (no bias tee), 400 device_required, 404 unknown_device, 409 not_live/device_busy |
| POST | `/api/control/baseband_filter` (T-067) | `{"bandwidth_hz", "device_id"?}` | `{tuning, device}` **(device action)** (validated against `device.baseband_filter`) | 501 unsupported (no selectable filter), 400 out_of_range/device_required, 404 unknown_device, 409 not_live/device_busy |
| POST | `/api/control/display` | any of `{"fft_size", "averaging", "rows_per_s", "window"}` (T-067; at least one) | `{display}` | 400 invalid |
| GET | `/api/control/scan[?f_lo_hz&f_hi_hz&dwell_s&step]` (T-452, `step` T-517) | – | `{scan, proposed}` — `proposed` prices the named sweep **without starting it**, `null` when none is named | 400 invalid (a range the device cannot reach), 409 not_live |
| POST | `/api/control/scan` (T-452) | `{"f_lo_hz"?, "f_hi_hz"?, "dwell_s"?, "step"?}`, or `{"resume": true}` | `{scan, proposed, device: {commissions, id, commissions_rate_hz}}` **(commissions retunes, and for a coarse step one rate change)** | 400 invalid, 409 refused (one already running / nothing to resume), 409 not_live |
| POST | `/api/control/scan/stop` (T-452) | `{}` (or empty) | `{scan}` — **never refused** | 409 not_live |
| POST | `/api/control/record/start` | `{"label"?, "max_s"?}` | `{recording}` | 409 refused (a content-forbidding class), conflict (already recording) |
| POST | `/api/control/record/stop` | `{}` (or empty) | `{recording}` (the stored `Recording`) | – |
| GET | `/api/bookmarks` | – | `{"bookmarks": [Bookmark, …]}` | – |
| POST | `/api/bookmarks` | `{"name", "f_center_hz", "kind"?, "bandwidth_hz"?, "note"?}` | `Bookmark` (`201`) | 400 invalid |
| GET | `/api/bookmarks/{id}` | – | `Bookmark` | 404 not_found |
| PUT | `/api/bookmarks/{id}` | any create field (`null` clears `bandwidth_hz`/`note`); `{"name"}` alone renames it | `Bookmark` | 400, 404 |
| DELETE | `/api/bookmarks/{id}` | – | `{"deleted": Bookmark}` | 404 |

`Bookmark`: `{id, kind ("marker"|"bookmark"), name, f_center_hz, bandwidth_hz, note, created_s, updated_s}`.

`tuning`: `{center_hz, sample_rate_hz, gains: {"<stage>": <dB>, …}, bias_tee, baseband_filter_hz}` (`bias_tee` (T-325) is `"unknown"`, `"off"` or `"on"` — three states, never a bool. `"unknown"` means nothing has reported a state: this server has not set one and the device does not have a bias tee. **It must not be read as off** — a bias tee left on into a passive or DC-shorted port is a hardware hazard, and an active antenna's LNA moves the noise floor, so unknown is not a claim that either is safe. Whether the control exists at all is `device.bias_tee`, not this field. `baseband_filter_hz`: `null` without that capability, or before it's been set explicitly — the device's own default is in force). `display`: `{fft_size, averaging, rows_per_s, window}` (`window`: `"hann"`, `"blackman-harris"` or `"flat-top"`; T-067). `run`: `{live, content_class, content_permitted, center_hz, sample_rate_hz, segment, replumbing, finished, capture, capture_note, display, recording}` — `segment` increments on every re-plumb (a retune or rate change into a window of another content class) and on every capture restart. **`capture` (T-508)** says whether the front end is delivering samples: `"running"`, `"recovering"` (capture failed — a device read error, e.g. a retune the device refused when it applied it, or a re-plumb that could not complete — and a new segment is being started or has not yet delivered a sample; the live edge is not advancing and that is the true state) or `"ended"` (the run is over; nothing more will arrive). `capture_note` is the cause while `recovering` or after an `ended` that was a failure, and `null` while `running` or after a requested stop or a recording's end. A recovery first re-sends the window the failed segment was built for, then the last window that delivered samples, and gives up after 5 attempts in a row that delivered nothing — so `"ended"` with a note is a front end that is really gone. `center_hz`/`sample_rate_hz` (and `tuning`) follow a recovery: a device that refused a retune and was put back on its previous window reports that window, not the refused one. A client must render `recovering` and `ended` on the surface itself; a frozen edge that looks live is the defect this field exists to prevent. `stats` counts `capture_failures`, `replumb_failures`, `capture_recoveries`, `segments_salvaged` (a re-plumb whose old state a straggling thread still held, completed on a fresh database connection instead of ending the run) and **`worker_panics` (T-541)** — pipeline threads that ended by panicking rather than returning. Always a defect, and served rather than kept internal for the same reason as `window_settle_timeouts`: a fault the system handled is still a fault an operator should be able to see. A panicking thread ends its segment, so it arrives as a `capture_failure` and a recovery (or, if it recurs on every segment, as `"ended"` with the panic named in `capture_note`) rather than as a reader silently missing from a run that still reads `running`. `recording`: `{active, id, label, center_hz, sample_rate_hz, samples, lost_samples, max_s, stored, ended}`.

**`display_limits` (T-067)** reports the bounds `POST /api/control/display` accepts, so the UI stops hard-coding hk-pipeline's `DISPLAY_*` constants: `{fft_size_min, fft_size_max, averaging_max, rows_per_s_min, rows_per_s_max, windows}` — `fft_size` must be a power of two in `[fft_size_min, fft_size_max]`, `averaging` in `[1, averaging_max]`, `rows_per_s` in `[rows_per_s_min, rows_per_s_max]`, `window` one of the `windows` list. `null` when this server has no running pipeline (503-class servers only; both live and replayed runs report it).

### The in-app survey sweep, and who wins when it and the user both want the radio (T-452)

`/api/control/scan` starts, prices and stops T-406's iterative scan **from the running app**, so the coverage map fills — grey turning lit — as the sweep steps, instead of a survey being something you could only ask for at server launch.

**`hk serve` still does not drive the scheduler, and that is the decision, not an oversight.** T-406 built the iterative scan as a dwell policy *over the scheduler*, and `hk serve` deliberately composes its run without one. Turning the scheduler on here would be a second composition path (the scheduler and the interactive observer are chosen when a segment is built, not while it runs), and it is not needed: the interactive run **already writes what a sweep needs**. `hk_pipeline`'s interactive observer closes **one dwell record per steady tune**, with that tune's own window and its own interval — which is exactly T-406's load-bearing requirement of *one record per step with its true band and interval*, the thing that stops a pass rasterising as "the whole band, the whole time". So the sweep is a **driver over the interactive retune path**: every step is the same gated `DeviceAction::Retune` a user's explicit tune is, through the same gate, recorded against the same `device_id`, and its coverage lands in the plane every other surface already reads. There is **no second accumulator and no second device path**. `hk run` / `hackriffd` keep `--survey-dwell`, which is the scheduler-driven form.

**Commissioning is a third classification, not a loophole.** `POST /api/control/scan` moves no front end within the call, so it is *not* one of the six device actions — and it is obviously not a view change either, because it commits this radio to hundreds of retunes over the next hour or two. Its answer and audit entry therefore carry `device: {commissions: "retune", id}`, never `{action, id}`: the log says which radio was committed without ever reading as if the request itself moved it. `/api/control/scan/stop` and the `GET` are plain view-side routes — surrendering the radio must never be refusable.

**The arbitration: the user wins, the sweep yields, and the sweep says so.**

> An explicit user device action always wins. The sweep yields at the step it was on, **keeps its place**, and reports what took the radio. The user resumes it or stops it.

"Explicit user device action" is not a guess — by the rule above, a client may only call the six device routes for an explicit user act, so anything arriving on one *is* the user acting. The sweep is in-process and reaches no route, so it cannot mistake itself for the user. The yield happens **before** the user's action is attempted, so the sweep has already stopped stepping by the time that action reaches the gate.

The alternatives were weighed and rejected: *refusing the user* while a sweep holds the radio would make T-444's retune-on-pan start failing for the ~80 minutes of a 6 GHz pass, on a route that must keep working; *letting the sweep step on and take the tune back* is the silent drop the honesty rule forbids — the user's tune stands for a few seconds and is then undone by something they cannot see (T-409's clamped-nudge lesson, one layer up).

A yield is therefore never silent, and never wrong in either direction:

- the **user's own response** carries the `scan.yielded` object their action caused — the answer to "why did my sweep stop" is in the reply that stopped it;
- `GET /api/control/scan` and `/api/control/state` both report `state: "yielded"` with the same object;
- a user action **refused before it reached the device** un-yields the sweep, because nothing took the radio;
- the **sweep's own step** can lose too: a *transient* refusal (`device_busy`, `conflict`, `timeout`) is retried on the same step a few times, and anything else — or a step that keeps failing — yields with that error and keeps its place. What a step never does is move on, because a skipped step would claim a band was swept when it was not.

**A step is a retune, with a retune's costs.** Each step is one `POST`-equivalent `set_center` and nothing else: a fine sweep never changes the rate, the gains or the filter (a coarse one changes the rate once, before its first step — above), so the pass is tiled at the span in force and every step is a single device action. Where the window's content class is unchanged that is a tune in place; a step across a class boundary re-plumbs the segment exactly as a user's retune across the same boundary does, around the still-open device — capture is not stopped, and the step just starts later, since the dwell is timed from when the retune returns. A range inside one class, which a band survey usually is, never re-plumbs.

**What the control shows before the button.** `GET /api/control/scan?f_lo_hz=…&f_hi_hz=…&dwell_s=…` prices a sweep without starting one, so the arithmetic is in front of the user rather than in the log afterwards. `proposed.budget.statement` is T-406's own sentence with this plan's numbers in it — *"401 steps × 12.0 s = a 4812.0 s pass over 5999.000 MHz (15.000 MHz per step); each band is listened to 12.0 s in every 4812.0 s (duty 0.249 %). A step catches anything on the air during its own dwell; it catches nothing during the other 4800.0 s, and spectrum the pass has not reached is unobserved, never quiet."* A 6 GHz sweep at a 15 s dwell is a commitment of about 80 minutes, and the control says so first.

**The step width: `step` is `"fine"` or `"coarse"` (T-517).** *Where today's small step came from:* the pass is tiled into slices of `sample rate × usable_fraction` (0.75), and a fine step tiles at the rate **in force** — so a live run left at the HackRF's 2 Msps floor (where a narrow selection snaps it, T-418) steps 1.5 MHz, **4000 steps** for 1 MHz–6 GHz, and the 2.4 Msps demo steps 1.8 MHz, 3334 steps. `usable_fraction` and `max_span_hz` (20 MHz) were never the limit; the window in force was. A **coarse** step tiles at the widest `rate × 2^k` the device supports **at which the run's detection/history bin width is unchanged** — from 2.4 Msps that is 19.2 Msps (14.4 MHz per step, **418 steps**; from 2 Msps, 16 Msps: 12 MHz, 501 steps), and at a 0.5 s dwell the full-range pass drops from ~28 min to ~3.5 min. The pipeline sizes its analysis FFT as `next_pow2(fs / 5 kHz)`, so a power-of-two multiple of the rate takes the same multiple of bins: **`plan.bin_hz` is identical fine vs coarse** (4687.5 Hz from 2.4 Msps), and a rate that would coarsen a bin (a fixed FFT override) is never chosen — coarse then equals fine. Coarse is fewer, wider windows, never blurrier data. The choice is an enum rather than a step in Hz because the step is not free: it is a device-achievable rate, and only the power-of-two ladder keeps the bins exact, so a free Hz value would only be snapped back to one of these. Omitting `step` is `"fine"`, today's behaviour. A coarse scan from a narrower window **sets its rate once, before its first retune**, through the same gated device path (`set_rate`, recorded against the same `device_id`); the start answer names it in `device.commissions_rate_hz` (`null` when the rate does not change), and a step whose window in force differs from the plan's (a resume after the user changed the rate while yielded) restores the plan's rate first, because a hop tiled at one rate and taken at another leaves holes. A server that cannot state its bin width refuses a coarse step (`409 refused`) rather than take it blind.

Shapes:

- `scan`: `{state: "idle"|"running"|"yielded", available, unavailable_reason, plan, budget, progress, yielded}`.
  - `plan`: `{f_lo_hz, f_hi_hz, dwell_s, recommended_dwell, sample_rate_hz, rate_in_force_hz, changes_rate, step, bin_hz, steps, warnings}` — `step` is `"fine"`/`"coarse"`; `sample_rate_hz` is the rate the pass is **tiled at** (the rate in force for fine; the wider coarse rate otherwise) and `rate_in_force_hz` the rate when it was priced, with `changes_rate` whether they differ; `bin_hz` is the detection/history bin width at `sample_rate_hz` (identical fine vs coarse by construction; `null` when the server cannot state it). The rest: — `recommended_dwell` is whether the dwell is inside the 10–30 s the survey is sized for; a dwell outside it **still runs** (the user asked for a configurable number, not a clamped one) and this is what lets the UI say it is unusual. For a fine step `sample_rate_hz` is the span **in force**: the pass is tiled at the window the run is actually capturing, and a fine sweep never changes the user's span or gains — the one device action per step is the retune (a coarse one adds the single rate change above). `warnings` say what compilation did to the request, clipping above all.
  - `budget`: `{steps, dwell_s, pass_s, revisit_s, span_hz, step_span_hz, duty, statement}`. `pass_s` **is** the revisit interval, and `duty` = `dwell/pass` — the honest headline of the trade, not a detection probability.
  - `progress`: `{step, steps, pass, steps_done, center_hz, started_s, step_started_s, next_step_in_s}`, `null` when idle.
  - `yielded`: `{to, at_s, step, detail}` — `to` is a `DeviceAction` name (`"retune"`, `"rate"`, …) for a user action, or `"step_failed"` when the sweep's own retune was refused. `null` unless yielded.
- `available` is `false`, with `unavailable_reason`, for a front end that cannot be retuned or states no tunable range — "nothing said" about a range is not a range, so the control is disabled with its reason rather than offering a button that would fail.
- `proposed` is a `{plan, budget}` pair, `null` when the request named neither a range nor a dwell. `f_lo_hz` and `f_hi_hz` **go together**: either alone is a half-stated range, refused rather than guessed at. Omitting both prices a sweep of everything this front end can tune.
- `resume: true` takes no range and no dwell — it continues the sweep that yielded, at the step it stopped on. Starting with a range while one is yielded replaces it; starting while one is **running** is `409 refused`, because two sweeps over one front end are two policies fighting for the same tune.

Contract (T-517): `crates/hk-cli/tests/api_contract.rs::a_coarse_sweep_step_is_fewer_windows_at_the_same_bin_width` prices the whole range fine vs coarse on the mock and asserts the step counts (3334 / 418), step spans, pass lengths, and `bin_hz` equal to `hk_pipeline::detection_bin_hz` in both — plus the refusals and `commissions_rate_hz`.

Contract: `crates/hk-cli/tests/api_contract.rs::a_survey_sweep_can_be_started_from_the_app_and_yields_to_the_user` drives the whole thing through the mock SDR device — prices a pass, starts it, watches the tune step and **per-step observation records appear with their own centres**, then asserts the user's retune succeeds *and* carries the yield it caused, that the user's tune is not taken back, that a refused user action un-yields, and that stopping is never refused.

### Pause is client view state — there is no pause route (T-347)

`POST /api/control/pause` and `POST /api/control/resume` **no longer exist** (they answer `404 not_found`), and `display` no longer carries `paused`. A client that wants to hold its view does so entirely on its own side.

**Why.** Those routes set `paused` on the **run**, and every connected client shares the run: one browser pressing Pause stopped the spectrum publisher for *all* of them. The flag never reached the device — capture, the ring and detection carried on (T-339) — so the "pause freezes the view, not the capture" half of the invariant held. The other half did not. The user's words are *"the UI's time window is **independent view state**"* (CLAUDE.md, "Time, the waterfall, and the live view"), and **a view state shared between browsers is not view state**: a run-wide boolean cannot represent N viewers, so no scoping of that flag could have been right.

**One mechanism for what the user calls one thing.** Before this there were two unrelated mechanisms for "hold the picture": a server-side publish-stop (Pause) and a client-side time cursor (scrub). They are now one. Holding the view *is* the time cursor — a view pinned to a fixed `[t − span, t]` instead of following the live edge — which is the same state a scrub or a time-region zoom produces, and the same state every surface already reads (`time.live`: the presence push subscribes only while following, the inventory window is derived from it, live-only notes change on it). Pause is therefore a *transition into the state scrubbing already used*, not a second thing:

| The user does | What changes |
|---|---|
| Presses Pause | the client's time cursor stops following the live edge, holding the span it is showing at the newest row it has |
| Scrubs or zooms in time | the same cursor moves to another instant/span |
| Presses Live | the cursor follows the live edge again |

Nothing in that column reaches a route. The rows keep being published for as long as the run lasts; a held view simply stops advancing over them, and the waterfall, boxes, scrubber and inventory lists all re-derive from the same cursor (T-337's one shared time axis).

**Telling the server you have stopped looking (room for T-348).** Wanting to save bandwidth and CPU while nobody is watching is legitimate — it matters on a handheld battery — but it is a property of a **connection**, not of the run. The connection-scoped mechanism already exists and needs no new route: a client that has stopped looking **closes its stream subscription** (and re-opens it on Live, backfilling from history as a fresh Live view already does), at which point the publisher stops encoding and writing for it entirely. If measurement later shows the remaining per-run cost dominates — the display STFT runs before the publish step, so a paused viewer never saved it even under the old flag — the honest control is a server-side low-power mode keyed on *no consumers attached*, which the stream registry can already answer, and which is an operator's world-state decision rather than a viewer's view-state one. What must not come back is a run-wide flag any client can set.

**The cross-client guarantee is tested as a cross-client property.** `crates/hk-cli/tests/api_contract.rs::one_clients_pause_never_freezes_another_clients_stream` opens **two** WebSocket consumers on one server and asserts the second keeps receiving rows with strictly advancing capture timestamps while the first is paused and then stops draining altogether. A single-client test cannot see this class of defect, which is why the original shipped.

### Device actions (T-343)

The six endpoints marked **(device action)** above — `center`, `rate`, `window`, `gains`, `bias_tee`, `baseband_filter` — are the only ones that **reach the front end**. Everything else changes what is shown or what is stored. The difference is on the wire, not a convention:

- **`device` on the answer and in the audit log.** A device action answers with `device: {action, id}` — `action` is `"retune"`, `"rate"`, `"window"`, `"gains"`, `"bias_tee"` or `"baseband_filter"`, and `id` is the front end's provenance `device_id` (e.g. `hackrf:<serial>`, `mock:<recorded id>`). The same object is written to the audit entry, whether the request succeeded or was refused, so the log always says **which device** a retune moved. `id` is `null` when the source reports no identity — "nothing said", never a placeholder. A view change (`display`, `record/*`, bookmarks, selections) carries **no `device` key at all**.
- **`device.device_id` on `/api/control/state`.** The same id, so a client can name the front end a retune would move *before* it asks for one.
- **`device.tuning_step` / `device.tuning_step_hz` on `/api/control/state` (T-341).** The centre-frequency granularity, three-valued like the bias tee: `"uniform"` with a step in Hz, or `"unknown"` with a `null` step when the source cannot say — **never read as 1 Hz and never as continuous**. It is the third axis of the achievable `(centre, span)` grid; [`GET /api/navigation`](#get-apinavigation--the-achievable-centre-span-grid-t-341) reports the whole grid, and this is the same fact beside the rest of the device's capabilities.
- **`409 device_busy`.** Only one process can hold an SDR, and inside this server device actions serialise on one gate **per front end** (T-511: two radios are two resources, so a busy one never refuses the other). A device action that cannot claim the front end within ~250 ms answers `409` with code `device_busy` and a message naming the device, the action holding it, and for how long. It does **not** race the holder to the driver, and it does not block for the length of a re-plumb. Distinct from `conflict` (the run's own state, e.g. a re-plumb in progress) and `not_live` (a replay). A client should report it, not retry into the race.

**A retune is not a view control.** `POST /api/control/center` re-derives the window's content class and, when the class or sample rate changes, stops and re-plumbs the running segment — tearing down and restarting its always-on readers. Pausing, scrubbing and zooming never reach the device; this does. A client must therefore call it only for an **explicit user action** (a frequency typed and submitted, a bookmark clicked, a retune button pressed, a region selected on the frequency navigator), **never as the continuation of a pan, a zoom or a drag**, and never automatically. In the web UI that rule is a type: `ui/src/app/centre/view.ts` takes a `DeviceAction` and is the only module besides the SDR control panel that names a device route; a pan that runs off the band edge leaves a *retune offer* for the user to accept.

#### Which radio: the device selector (T-511)

A run may hold **several** front ends. CLAUDE.md requires the source layer, scheduler and inventory to be N-source-capable; T-510 made one `Pipeline` spawn N `{source, ring, capture thread}` sets; this is the serving half. `ApiState::live_controls` is a collection keyed by `device_id` rather than one `Option`, and every route that reaches a radio takes an optional `"device_id"` body field.

| The request says | With one front end | With several |
|---|---|---|
| no `device_id` | that front end (**unchanged** — every existing client) | `400 device_required`, listing the ids |
| `device_id` of a held front end | that one (404 if it is not the one) | that one |
| `device_id` naming nothing here | `404 unknown_device`, listing what is held | `404 unknown_device`, listing what is held |
| `device_id` that is not a string | `400 invalid` | `400 invalid` |

**Why an omitted selector is refused rather than defaulted.** Picking "the first composed" would be a plausible answer to a question the caller never asked, and the thing it would do is *move a radio*. It is the same rule as `BiasTee::Unknown` not being `Off` and `Coverage::Unobserved` not being quiet: where nothing was said, nothing is invented. A refusal costs a client one field; a default costs the user a band they were listening to.

**What is unchanged.** `device.id` on the answer and in the audit entry is the front end the selector **resolved to** — with one radio, the same string as before. `GET /api/navigation`'s `windows` needed no change at all: it has spoken in lists since T-340, and its `frequency.current` is still explicitly *one* device's tuned state (the default front end), which is why a client places segments from `windows`. A pan or a wheel still reaches **no route**, so nothing here widens what navigation can command (T-340's spy-client assertions).

**One capture at a time is per device.** Each front end carries its own gate, so a retune of one radio is never refused because another is busy, and `409 device_busy` still names the device that is held.

**Discovering the ids.** `GET /api/control/state` carries `devices`: `[{device_id, device, tuning}, …]` in composition order — `[]` on a replay, one entry on a single-SDR run. Its singular `device`/`tuning` are that one entry on a single-SDR run and `null` when the run holds more than one, because then there is no "the" device to describe. `GET /api/navigation`'s `windows` is the same enumeration seen as capture windows.

#### A window is one device action (T-529)

A user retune names a **capture configuration**: a centre *and* a span. `POST /api/control/window` commits both as one device action — one content-class derivation, at most one re-plumb, one audit entry — and both fields are **required**.

It exists because committing the pair as `POST /api/control/rate` then `POST /api/control/center` is not a formality. Each single-field route completes the half the caller did not name **from the tuning in force**, so two posts command *two* windows, and the intermediate one — the old centre at the new rate — is a window no user asked for. It is observable and it costs something:

- `GET /api/control/state` reports it in between, and a client's own poll can read it;
- a whole segment is captured at it, with spectrum headers and provenance to match, so the **coverage map records "observed"** over a band chosen by an HTTP artefact rather than by the user — `Coverage::Observed` is supposed to mean the radio was pointed there deliberately;
- the run re-plumbs twice for one press, tearing the always-on readers down and back up an extra time;
- and the second post races the first's new segment. A device refusal arriving while the second re-plumb is already queued is taken by the pipeline's request-first branch and counted as neither a capture failure nor a recovery — which is how T-508's one-shot device fault could land on either path, and why `canvas-journey` test 5 flaked.

The single-field routes are unchanged and still right for a caller that genuinely means one field: a nudge, a bookmark, a typed frequency, the SDR panel's rate picker, and [the survey sweep](#the-in-app-survey-sweep-and-who-wins-when-it-and-the-user-both-want-the-radio-t-452), whose steps are centre-only by design. A window whose centre *or* rate the front end cannot reach is refused whole (`400 out_of_range`) with neither half applied, which two posts cannot do — by the time the second is refused the first has already moved the radio.

**Region-select on the frequency navigator is the one gesture that commands the radio (T-392).** The user's invariant (CLAUDE.md, the frequency navigator): *selecting a region on it **retunes the front end to cover that region**, snapped to an achievable config, not merely offering a retune — because unlike time, which is always a view over already-captured data, a frequency outside the current window can only be reached by tuning there.* This is not a new path to the device: it builds the same gated, audited `DeviceAction` the retune button always built, with `source: "navigator"` and the device's `device_id` recorded. Pan and wheel on either navigator still reach nothing, at any distance.

A region resolves against [`GET /api/navigation`](#get-apinavigation--the-achievable-centre-span-grid-t-341) to one capture configuration — the region's centre snapped to `center_step_hz`, and the **smallest** `spans_hz` entry that still covers the region from that snapped centre — and is applied as **one** device call, `POST /api/control/window` with both halves (T-529; see [A window is one device action](#a-window-is-one-device-action-t-529)) — followed by the view-only `POST /api/control/display` described below. The client **refuses only when no achievable configuration can capture the region**: a centre outside every `ranges_hz` band (on a replay this is the recording's own extent, since `replay_capabilities` reports the recording as the band), or a span wider than `max_live_span_hz`. Everything else retunes — reporting "outside the tuned window, retune yourself" is the behaviour this replaced. The request goes out **on pointer release**, with no confirmation step; a *pan* past the band edge still leaves a retune offer for the user to accept, because a pan names no destination to fire on.

**A narrow selection raises resolution; it never asks for an impossible narrow capture (T-418).** The user's principle: *"Detail on a narrow view comes from resolution/decimation, never from an impossible narrow capture."* A HackRF's minimum sample rate is 2 Msps, so **a sub-2-MHz window cannot be captured** — `spans_hz.min` *is* the answer for a narrower region, and no retune will produce a tighter one. Selecting a narrow range therefore does two things, and needs both:

1. **The window snaps to the narrowest achievable one, placed off DC.** The centre is *not* the region's centre: a target at the exact centre lands on the tuner's own DC/LO leakage spike and cannot be cleanly demodulated. The client offsets it by `spans_hz`/4 — the midpoint of the usable half-band, maximally far from both the LO spike at DC and the anti-alias roll-off at Nyquist — bounded by the slack the selection leaves in the window and by half a `center_step_hz` for the snap that follows. A selection too wide a fraction of the narrowest covering window has no placement that clears DC of it; the window is **not** widened to buy the dodge (that would cost the very resolution this path exists to gain), and the UI says DC falls inside the selection rather than implying a clean window.
2. **And the transform lengthens**, via `POST /api/control/display` with `fft_size` — the smallest power of two inside `display_limits` that puts one measured bin per pixel across the selection. This is where every hertz of detail below the device's rate floor comes from. A longer FFT is a **genuine measurement** over more samples, not a coarse one stretched; it is not a device action (no `device` key, no re-plumb, no settle gap), which is why it is the first lever rather than the last. Past `fft_size_max` the selection gets fewer bins than pixels and the client nearest-repeats them — the UI states that in words, because a view must never imply detail the front end did not capture.

**Whether the radio moves is no longer a geometry test.** "Inside the tuned window" used to mean "a pure view zoom, no request at all", which got narrowing backwards: a 200 kHz selection inside a 2.4 MHz window is inside, so nothing changed and the zoom bought no detail. A selection now reaches the front end in exactly three cases, each self-limiting so repeating the same selection settles rather than moving the radio again: it is **not wholly inside** the window in force; a **narrower achievable window** exists; or **DC falls inside the selection** and a placement exists that clears it. When none holds — the window is already the least one that covers the selection and DC is clear of it — the radio is not touched and only the transform moves.

`device.baseband_filter` (inside the `caps_json`, T-067) describes the selectable bandwidths: `{"min_hz", "max_hz"}` for a continuous range or `{"values_hz": [...]}` for a discrete list (e.g. the HackRF's MAX2837 filter steps), or `null` when the device has no selectable filter.

**Content gating is off by default (T-143)** and lives in the pipeline and repository, not the API. Unless `HK_CONTENT_GATING=1` opts in, every class permits content: recordings, audio, chains, decodes and identities flow regardless of band, and `content_class`/`source_class` are informational. When opted in, a retune re-derives the window's content class and re-plumbs at a block boundary, and recordings are refused under a class that forbids content; the API never opens content itself (bookmarks are user metadata only). `transmit.available` is always `false`.

## Selections (T-052)

Persisted named region selections (several at once, survive a restart, stored in the run's database). Errors: `{"error", "code"}` with `invalid`, `not_found`, `conflict`, `unavailable`.

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/api/selections` | – | `{"selections": [Selection, …]}` (creation order) |
| POST | `/api/selections` | `{"name", "f_lo", "f_hi", "id"?, "t_lo"?, "t_hi"?, "notes"?, "tags"?}` | `Selection` (`201`); `409 conflict` when `id` already exists |
| GET | `/api/selections/{id}` | – | `Selection` |
| PUT | `/api/selections/{id}` | any create field but `id` (`null` clears `t_lo`/`t_hi`/`notes`/`tags`) | `Selection` |
| DELETE | `/api/selections/{id}` | – | `{"deleted": Selection}` |
| POST | `/api/selections/{id}/links` | `{"kind" ("demodulation"|"recording"|"bitstream"|"inspection"), "target", "note"?}` | `Selection` (`201`) |
| GET | `/api/selections/{id}/watch` | – | the region watch's alerts and the activity it did not alert on (T-166) |

A client may choose `id` (any UUID) so an optimistic/offline-created selection keeps its identity when it syncs; a retried create then answers `409` and the client should `PUT` instead. `Selection`: `{id, name, f_lo, f_hi, t_lo, t_hi (null = any time), notes, tags, watch, links: [{kind, target, t, note}] (oldest first), created, updated}`.

### Region watch (T-166, ADR-0013 §4.9 gap 9)

`watch` on a selection arms "alert on new activity" over its extent: `{"enabled": true}` to arm, `{"enabled": false}` or `null` to disarm; absent on create or update leaves it as it was, and `null` is the only accepted non-object. **There is no threshold to set on purpose** — what counts as activity is measured, never dialled in. A selection bounded in time (`t_lo`/`t_hi`) is watched only inside that window.

An armed watch offers every emission the pipeline first sights inside the extent to the rule, and raises, for each one it accepts, an `Anomaly` (kind `new-emitter`, `baseline_ref` `region-watch:v1;selection=<uuid>;emitter=<uuid>`, `detector_version` `hk-context.region-watch@1`) with an `Explanation` whose `cause.kind` is `own-history` and whose `description` is the reasoning in words, plus a message on the **`anomalies` stream** in the usual list-row shape with an extra `metadata.watch` block (`{selection_id, name, f_lo, f_hi, reason}`). A watch alert carries `alarm: null`: it is not a baseline novelty alarm, has no baseline, and its `score` of 1 means "new inside the extent you asked to watch", not a measured novelty.

**It never alerts on a row the T-219 relationship rules explain as another row** — one suppressed by a Confirmed entry, a duplicate of a stronger candidate, or attributed as an image, harmonic or intermod of a confirmed source. Such a row is a signal already known, or the receiver's own artifact, and reporting it as new activity would train the user to ignore the watch. A relationship lookup that fails suppresses the alert rather than raising it. Each emitter alerts at most once per watch.

**Alerts carry their reasoning and are reversible, never an automatic action.** Raising one tunes nothing, records nothing and changes no other row; alerts are dismissed and re-opened through `/api/anomalies/{id}/dismiss|reopen` like any other anomaly; and disarming the watch stops new alerts while keeping every alert already raised, with its reasoning and history. Suppressed activity is disclosed rather than dropped silently, the same stance ADR-0012 §7.3 takes towards alarm suppressions.

`GET /api/selections/{id}/watch` answers `{selection_id, watch, armed, alerts, suppressed, alerted_total, suppressed_total}`. `alerts[]` is `{anomaly_id, emitter, f_lo, f_hi, t, reason}` and `suppressed[]` is `{emitter, reason ("deferred" | "already-alerted"), relation ("suppressed-by" | "duplicate-of" | "artifact-of" | "retune-sibling-of" | null), artifact ("image" | "harmonic" | "intermod" | null), source, t, explanation}`, both oldest first and bounded (256 alerts, 64 suppressed). Counters are per run and also appear in `/api/status` as `watch_alerts` and `watch_suppressed`. `404 not_found` for an unknown selection, `503 unavailable` with no watch service.

- **`t` is bare and carries Unix seconds** — the units convention's default, so no `_ns` rename applies here (T-370 audit): `hk_api::selections::WatchAlertView`/`WatchSkipView` declare `t: f64`, and `crates/hk-cli/src/pipeline.rs`'s `PipelineWatch::report` converts each `hk_pipeline::alarms::WatchAlertRecord`/`WatchSkipRecord`'s raw-nanosecond `Timestamp` to seconds (`secs(a.t)`) before it ever reaches this crate — the internal record and the view served here are different types, one nanosecond-native, one already seconds. This route was previously unreachable by `every_serialized_time_declares_its_unit` (no selection existed for it to address on a fresh server), so the value had never been swept and asserted; the sweep now creates one first and covers it.

## Annotations (T-816, MAP-16)

Durable, **human-authored** time–frequency notes — a text note, a box or a marker — that a researcher draws on the canvas ([docs/25 §5](25-spectrum-research-workflow.md), the normative store contract in §10, [ADR-0023](adr/0023-map-ui-and-research-state.md)). Stored in the run's user-metadata database beside bookmarks and selections, so they survive a restart. Errors: `{"error", "code"}` with `invalid`, `not_found`, `conflict`, `unavailable`; `405` with `Allow` for a wrong method; `401` before dispatch.

| Method | Path | Body / query | Response |
|---|---|---|---|
| GET | `/api/annotations` | `f_lo`, `f_hi` (Hz), `t0`, `t1` (capture-clock Unix s) **required**; `limit`? (default 200, max 2000); `cursor`? | `{"window", "annotations": [Annotation, …], "count", "matched", "limit", "next_cursor"}` |
| POST | `/api/annotations` | `{"kind" ("text"\|"box"\|"marker"), "f_lo_hz", "f_hi_hz", "t0_s", "t1_s", "label", "body"?, "collection_id"?, "id"?, "view"}` | `Annotation` (`201`); `409 conflict` when `id` already exists |
| GET | `/api/annotations/{id}` | – | `Annotation` |
| PUT | `/api/annotations/{id}` | any create field but `id` (`null` clears `body`/`collection_id`); a `view` re-stamps provenance | `Annotation` |
| DELETE | `/api/annotations/{id}` | – | `{"deleted": Annotation}` |

`Annotation`: `{id, collection_id, kind, f_lo_hz, f_hi_hz, t0_s, t1_s, label, body, author, provenance, created_s, updated_s}`, with `provenance` = `{device_id, center_hz, span_hz, sample_rate_hz, t_capture: [t0_s, t1_s], tier ("live-iq"|"spectrum-history"|"survey-overview"), authored_s, actor, authored: true}`.

- **The window is required and the answer is paged** (the `/api/events` contract): an annotation is listed when its box intersects `[f_lo, f_hi] × [t0, t1]` (closed, so a zero-area note on the edge is inside), newest capture time first (`t1_s`, then `t0_s`, then id). `count` is this page, `matched` the whole window, and `next_cursor` (an opaque offset string, `null` on the last page) fetches the next.
- **Geometry.** `0 <= f_lo_hz <= f_hi_hz`, `t0_s <= t1_s`; a `box` needs a positive extent in both axes, while a `text` note or `marker` may be a zero-area point. `label` is 1–120 characters (trimmed), `body` at most 4000, `collection_id` a UUID (the MAP-17 collection; not yet checked for existence). `t0_s`/`t1_s` are **capture-clock** times and fixed: a human-set extent does not grow to the live edge.
- **Provenance is stamped by the server; the client sends only `view`** — `{center_hz, span_hz, t_capture: [t0_s, t1_s], tier, device_id?}`, the view context it was on (all but `device_id` required). The server adds `actor` and `author` (the token fingerprint `tok-…`, **never the token**), `authored_s` (wall clock — when the human acted, never compared with `t_capture`, which is when the air was), `authored: true`, and `sample_rate_hz` **only** when this run holds the device `device_id` names (otherwise `null`, never the primary radio standing in). A body carrying `provenance`, `author`, `actor`, `authored`, `authored_s`, `created_s` or `updated_s` — at the top level or inside `view` — is `400 invalid`: provenance is evidence, not input.
- **Audited** as `annotation_create`, `annotation_update`, `annotation_delete` (token id, peer, request, old/new, status), with **no `device` key**: authoring is a view act and reaches no radio. Without an audit log every mutating route is `503 unavailable`; without a store, every route is.
- **Never detection input.** An annotation mints no candidate, moves no threshold, confirms no emitter and never pre-populates the inventory; it is a different object from the §2.13 machine annotation and from a Confirmed emitter (docs/25 §5).
- **SigMF-adjacent export shape** ([docs/sigmf-extension.md](sigmf-extension.md), `hk_model::AuthoredAnnotation::to_sigmf`): a SigMF `annotations` entry with `core:sample_start`/`core:sample_count` (from `t0_s`/`t1_s` against the recording's start and rate), `core:freq_lower_edge`/`core:freq_upper_edge`, `core:label`, `core:comment` (from `body`), plus a `hackriff:annotation` block (`authored: true`, `kind`, `id`, `author`, `collection_id`, `provenance`) — structurally distinct from `hackriff:truth`, so a researcher's notes can never contaminate a fixture's hidden truth list. The export route itself is MAP-23.

## Output recordings (T-061)

Record a selection's, emitter's or band's bits, symbols, WAV audio and/or IQ to files on the device; stop, list, download.

| Method | Path | Body | Response |
|---|---|---|---|
| POST | `/api/outputs/record/start` | `{"selection_id" \| "emitter_id" \| "band": {"f_lo", "f_hi"}, "kinds": ["bits"\|"symbols"\|"audio"\|"iq", …], "max_s"?, "max_bytes"?}` | `{"recording": Session}` |
| POST | `/api/outputs/record/stop` | `{"id"}` | `{"recording": Session}` (once every file is finalised) |
| GET | `/api/outputs` | – | `{"recordings": [Session, …]}` (newest first) |
| GET | `/api/outputs/{id}/files/{name}` | – (`?token=` accepted: plain download links) | the file's bytes (`Content-Disposition: attachment`) |

Give exactly one of `selection_id`/`emitter_id`/`band` (else `400 invalid`); `kinds` must be a non-empty array of `"bits"`/`"symbols"`/`"audio"`/`"iq"`. `507 quota` when the global output quota is full, `503 busy` at the chain-budget cap, `404 not_found` for an unknown target/id, or whatever the underlying opener refuses with (e.g. `403` for a restricted band).

`Session`: `{id, active, selection_id, emitter_id, f_lo_hz, f_hi_hz, kinds, max_s, max_bytes, bytes, started_at, elapsed_s, ended, links_saved, files: [{kind, file, sidecar, extra_files, state, bytes, records, dropped_records, message, recording_id, bitstream_id, url, sidecar_url, extra_urls}]}` — `url`/`sidecar_url`/`extra_urls` are the matching `/api/outputs/{id}/files/{name}` download paths, to which a browser appends `?token=`.

## Analyze / synthesize decoder (T-190, T-546)

`POST /api/analyze` is the MUI "Analyze / synthesize decoder" action (docs/14, `docs/15-decoder-synthesis.md` §7): point it at a signal and it answers **why that signal's decoding pipeline was chosen, in terms of what was measured**.

For an **`emitter_id`** target it answers `200` with the emitter's latest analysis — the `emitter_synthesis` row (ADR-0015 §5.4) the pipeline wrote while it ran. The route **reads**; it never starts DSP, and every call is answered synchronously. Today the only producer is the trunking chain (`hk_pipeline::synth`), so a confirmed control channel has an answer and most emitters do not.

**`not-searched` is not `unknown`** (ADR-0021 §7A.4). An emitter no analysis has run on answers `200` with `pipeline: null` and `resolution: {"kind": "not-searched", "summary"}` — *un-looked-at*, which is a different fact from *looked at and found nothing*, and the two are never rendered alike. This is the decode-side statement of the canvas's grey rule: a client that shows them the same way has the same defect as one that paints unobserved spectrum as quiet.

**`selection_id` and `band` targets still answer `501`.** The general region-analyze engine (ADR-0015 §5.1: a queued job that acquires IQ from the ring or live and searches demod/framing/FEC structure) is not built; only an emitter the pipeline has already analysed has an answer to give.

| Method | Path | Body | Response |
|---|---|---|---|
| POST | `/api/analyze` | `{"emitter_id"}` | `200 AnalyzeResult` |
| POST | `/api/analyze` | `{"selection_id"} \| {"band": {"f_lo", "f_hi", "t_lo"?, "t_hi"?}}` | `501 {"error", "code": "not_implemented"}` once the target validates |

`AnalyzeResult`: `{emitter_id, provenance ("synthesized by output analysis"), engine, t_ns, verdict, stage_reached, pipeline, evidence: [StageEvidence], trace: [TraceNode], resolution, receiver}`. For a never-analysed emitter only `emitter_id`, `pipeline` (null), `evidence` (empty), `trace` (empty) and `resolution` are present.

- `verdict` (ADR-0015 §3.4, how deep the search got, never what it means): `energy` → `demodulated` → `clocked` → `framed` → `checked` → `solved`.
- `stage_reached` (ADR-0015 §1.1): `s0-channel`, `s1-demod`, `s2-clock`, `s3-bits`, `s4-framing`, `s5-check`, `s6-fields`.
- `pipeline`: `{demod, decode, params: [[name, value]], summary}`. `summary` is backend-rendered — the UI does no wording logic.
- `StageEvidence`: `{stage, metric, raw, n, bits, summary}`. `metric` uses the ADR-0015 §2.1 vocabulary; `bits` is significance against that metric's null; `n` is the support (symbols, bursts or distinct frames).
- `TraceNode` (ADR-0021 §2.1): `{id, parent?, stage, choice, family, seed_source, measured?, outcome, tried, evaluations, summary}`. **`tried` is served, never inferred**, and `measured` is null for exactly the not-tried outcomes: a hypothesis that was *deferred* or has *no block in this build* is a different answer from one that was measured and scored below its floor, and a client must not have to guess which. `outcome` is a closed enum — tried: `survived`, `pruned-floor`, `pruned-bound`, `pruned-beam`, `evaluated-worse`, `refined-into`, `memoised`; not tried: `deferred-prior`, `deferred-budget`, `unsupported`, `not-applicable`, `refused-power`.
- `resolution` (ADR-0021 §7A.2), present whenever the verdict is below `solved`: `{kind, deepest_verdict?, reason?, summary}`. `kind` ∈ `unknown | structured-unidentified | unsupported-structure | not-searched`; `reason` ∈ `no-signal | nothing-scored | tied | budget-exhausted | unsupported-structure`.
- `receiver` (docs/19 §7.6a): `{grid_hz, offset_hz, concentration, ppm}` — the receiver's **fitted** offset from the channel grid, a property of the radio rather than of the signal. It is known **modulo the grid spacing**, which is all a periodic grid can say.

Give exactly one of `selection_id`/`emitter_id`/`band`, else `400 invalid`; an unknown field anywhere in the body (or in `band`) is `400 invalid`; `band.f_lo >= band.f_hi` is `400 invalid`. `404 not_found` for an unknown selection or emitter, including a malformed id — `{id}` may name an entity since merged into another and resolves to the live emitter, exactly like `GET /api/inventory/{id}`. `503 unavailable` when this server has no selection store or no inventory. Authenticated and audited (`analyze`) like every other mutating control route (same bearer-token, header-only and cross-origin rules); the emitter form records nothing and changes nothing.

## IQ capture buffer (T-157, T-178)

The always-on raw-IQ history behind the Capture timeline (ADR-0013 §4 API gap 1). While a source runs, every block the ring receives is kept on disk with its tuning and gain provenance, oldest first out. The history **survives restarts**, and any span of it can be exported as a SigMF recording.

There is no record button; `POST /api/outputs/record/start` still records forward. Storage, eviction and recovery live in `hk_store::iqbuffer` ([ADR-0014](adr/0014-iq-capture-ring.md)), and the per-segment ring reader in `hk_pipeline::iqbuffer`. These routes only validate, route and audit (`crates/hk-api/src/iqbuffer.rs`).

- **Storage: a pre-allocated ring** (`<data dir>/iqbuffer/`).
  - **Files.** `ring.ci8` holds `slot_count` fixed-size slots (a sixteenth of the quota, 64 KiB..64 MiB each) of interleaved ci8 (2 bytes/sample, as captured). `ring.journal` is a small CRC-framed index. `ring.lock` stops two runs sharing a directory.
  - **Allocation.** The whole ring is allocated when the run starts: `F_PREALLOCATE` on macOS, `fallocate` on Linux, or a sparse file where neither works (`preallocated: false`). Allocation writes no data: 8 GiB took 0.27 s on the dev Mac.
  - **In the background.** Opening the ring (recovery, then allocation in steps of about 1 GiB) runs on its own thread, so neither the run's start nor the API waits for a large quota. Until it completes the status answers `enabled: false`, `allocation: "allocating"` and `allocation_progress` (0..1); clips answer `503 unavailable`. **Captured samples are not buffered while allocating.** Each segment's reader first waits up to 2 s for the ring to open, so a ring that opens quickly (any quota up to a few GiB, in milliseconds) buffers from the first block. After that it reads and discards until the ring opens, and buffering starts with the next block. Capture never waits for this reader.
    - **Every discarded sample is counted** (T-217): `allocation_skipped_samples` is the total captured while there was no writer yet, so a large ring's first minutes holding no IQ is visible in the status, not a silent gap. It is set by the feeder (`hk_pipeline::iqbuffer`, which sees these blocks before the ring exists), stays 0 for a ring that opens inside the 2 s wait (the common case, any quota up to a few GiB), and keeps its value once the ring opens (`allocation` moves past `"allocating"`) so the explanation persists.
  - **Locked.** If another process holds `ring.lock`, the buffer is disabled with `allocation: "locked"` and a `reason`; the run carries on.
  - **Newer format.** A ring whose journal header has a newer version than this build reads is left untouched (never wiped): `enabled: false`, `allocation: "incompatible"`, and a `reason`. An older format or another slot size resets the ring.
  - **Overwrite in place.** A new slot overwrites the oldest one, so the file count and sizes never change while a run is open, and every position is rewritten in turn.
  - **Persistence.** Everything written is made durable (checkpointed) at least every second, when a slot fills and when the run stops. A restart recovers the segments and continues the ring. A crash loses at most the last unsynced second.
  - **Torn data.** Recovery re-checks the newest slots' CRC-32. A torn slot is discarded together with anything newer, as is a torn journal tail.
  - **Failed fsync.** The bytes written since the last checkpoint may never reach the disk, so they are **poisoned**: removed from the index at once (no clip exports them), never sealed or recovered, and rewritten by the next writes. Counted in `sync_errors` and `poisoned_samples`; the segment ends there.
  - **Clips** are ordinary recordings and are never evicted.
- **Runs.** Every start of the buffer is a new `run`. Stream indices (`global_index`) restart with each run, and a replayed recording repeats its sample-clock times, so segments carry their `run`.
- **Quota changed between runs.**
  - More slots of the same size: everything is kept.
  - Fewer slots: the slots that still fit are kept, and the rest are discarded (`discarded_slots`).
  - A different slot size (e.g. 256 MiB to 4 GiB): the ring resets.
- **Segments** are contiguous runs of samples under one provenance. A new segment starts on every provenance change (retune, rate, gain, filter, overload), source gap, settle skip, ring overrun or discontinuity flag, so **a retune is always a segment boundary**. Times are on the **sample clock** (ADR-0012 §0: the time the captured block carried; a replayed recording reports the recording's time).
- **Retention: oldest first**, whichever limit is reached first:
  - the retention window (`t1 − t0` on the sample clock, trimmed to the sample, and kept across restarts);
  - the ring (a reused slot evicts what it held).

  Every eviction is counted in `evicted`.
- **Configuration.** On for every run except a lossless (unpaced) replay, which is a recording already.
  - **Library default: off.** `hk_pipeline::PipelineConfig::new` (the pipeline library's own constructor, used directly by tests and any other caller) sets `IqBufferConfig { enabled: Some(false), .. }`: no library caller buffers IQ unless it asks to. `hk serve`, `hk run` and `hackriffd` are what turn it on, over that same `PipelineConfig`: their `config_for` overwrites `iq_buffer` with `IqBufferConfig::from_env()` and then applies `--iq-retention`/`HK_IQ_RETENTION` and `--iq-buffer-max`/`HK_IQ_BUFFER_MAX` on top (below). `hk_store::iqbuffer::IqBufferConfig::default()` on its own (without going through `PipelineConfig::new`) is a plain library default, not the CLI's: `enabled: None` (buffers unless the run is a lossless replay) at the 2 min retention below — a caller that builds `IqBufferConfig` directly, rather than starting from `PipelineConfig::new`, gets that instead.
  - `hk serve --iq-retention <DURATION>` (env `HK_IQ_RETENTION`): the retention window, e.g. `90s`, `2m`, `1h`. Default **`2m`**; `0` or `off` disables the buffer. `hackriffd` takes the same flags.
  - `--iq-buffer-max <SIZE>` (env `HK_IQ_BUFFER_MAX`): an optional hard size cap, e.g. `512MiB`, `8GiB`.
  - **Quota** `quota_bytes = min(retention × the device's highest sample rate × 2 bytes/sample, max)`, at least two slots; a recording replayed uses its own rate. The ring (`allocated_bytes`, the whole slots within the quota) is **allocated on disk up front**, whatever is captured.
- **Disk impact.** The disk space is taken at start and stays taken.

  | Retention | Device rate | Disk |
  |---|---|---|
  | `2m` (default) | HackRF One, 20 Msps | 4.8 GB |
  | `1h` | 20 Msps | **144 GB** |
  | `1h` | 2 Msps | 14.4 GB |

  The quota is sized by the device's *highest* rate, not the rate in use. So a server running `--iq-retention 1h` (staging) should also set `--iq-buffer-max` to the space it can give, e.g. `--iq-buffer-max 16GiB`: at 2.4 Msps that holds about 57 min.
  - `HK_IQ_BUFFER=0` disables the buffer and `=1` forces it on (lossless replays too). `HK_IQ_BUFFER_MIN_FREE` (a size) sets the free-space floor, `HK_IQ_BUFFER_CLIP_MAX` the largest clip (default 256 MiB).
- **Disk and flash writes.** The writer sustains `2 × rate` bytes/s whatever the retention: 4.8 MB/s (≈ 415 GB/day) at 2.4 Msps, 40 MB/s (≈ 3.5 TB/day) at 20 Msps. Retention and cap bound the space used, not the wear. On eMMC, SD or a small NVMe that is a real endurance cost (a 600 TBW drive lasts about 6 months at 20 Msps around the clock), so shorten the retention, sample slower, or use `--iq-retention off` on storage with limited endurance.
- **Free space.** The buffer never fills the disk. The free-space floor (`min_free_bytes`) is by default 10 % of the filesystem, within 2..8 GiB.
  - **At allocation**, the ring must fit above the floor; a ring's own existing file counts as available.
    - If it does not fit, the ring is **shrunk** to the whole slots that fit (`allocation: "shrunk"`, `allocated_bytes < quota_bytes`).
    - Below two slots it is **refused**: `enabled: false`, `allocation: "refused"`, and a `reason` naming the bytes needed, the floor and the free space.
  - A failed write backs further writes off, 100 ms doubling to 10 s (`write_errors`, `failed_samples`, `error`). Nothing it failed to store is indexed.
  - **Sparse ring** (`preallocated: false`), whose space is only taken as it fills: writing **pauses** while another slot would leave less than the floor free, and resumes as soon as there is room. Each pause is counted (`paused`, `pauses`, `paused_samples`) and shows as a gap.
- **Never blocks capture.** The buffer is one more ring reader on its own thread; the ring never waits for it on a live source. A disk too slow for the rate laps the reader: the lost samples are counted in `dropped_samples` and show as a gap before the next segment.
- **Content rule.** Only when `HK_CONTENT_GATING=1`: a block whose window's class forbids content is not stored (`gated_samples`), as the manual recorder refuses it.

| Method | Path | Body / query | Response |
|---|---|---|---|
| GET | `/api/iqbuffer` | `?[t0=<unix s>][&t1=<unix s>][&limit=1..10000, default 1000]` | `IqBufferStatus` |
| POST | `/api/iqbuffer/clip` | one range: `{"t0", "t1"}` (Unix s), `{"t0_ns", "t1_ns"}` (integer Unix ns) or `{"global_index", "samples"}`; plus `"band"?: {"f_lo", "f_hi"}, "label"?, "run"?` | `{"recording": Clip}` (audited `iqbuffer_clip`) |

**`IqBufferStatus`**: `{enabled, reason, dir, retention_s, max_bytes, quota_bytes, max_clip_bytes, chunk_bytes, chunk_files, slot_count, allocated_bytes, allocation, allocation_progress, preallocated, persisted, run, recovered_segments, discarded_slots, head_slot, head_offset_bytes, wrap_count, fs_free_bytes, fs_total_bytes, min_free_bytes, paused, pauses, paused_samples, t0, t1, span_s, bytes, disk_bytes, samples, segments_total, segments_omitted, segments: [Segment], gaps: [Gap], evicted: {chunks, segments, samples, bytes}, dropped_samples, gated_samples, write_errors, failed_samples, sync_errors, poisoned_samples, allocation_skipped_samples, error}`.
- `enabled: false` with a `reason` when the run has no buffer (disabled, lossless replay, still allocating, allocation refused, locked by another process, a newer ring format, or the ring could not open). Every count is then 0.
- **Sizes.**
  - `retention_s` is the window, `max_bytes` the hard cap (`null` without one), and `quota_bytes` the quota the configuration asks for.
  - `chunk_bytes` is the slot size, `slot_count` the slots of the ring, and `allocated_bytes` = `slot_count × chunk_bytes`, the ring file's fixed size (≤ `quota_bytes`).
  - `chunk_files` counts the slots holding retained samples.
- **Allocation.** `allocation` is one of:
  - `"full"`, or `"shrunk"` (not enough free space for the quota): the ring is open;
  - `"allocating"`: the ring is still opening in the background (`enabled: false`);
  - `"refused"` (not even two slots fit), `"locked"` (another process holds the ring) or `"incompatible"` (a newer ring format, left untouched): disabled;
  - `null` when disabled for another reason.

  `allocation_progress` is the allocated fraction (0..1) while allocating, 1 once the ring is open, and `null` without a ring. `preallocated` is false when the filesystem only made a sparse file.
- **Persistence.** `persisted` is always true when enabled. `run` is this run's number. `recovered_segments` counts the segments recovered from earlier runs at start, and `discarded_slots` the slots recovery dropped (unsealed, torn, failed CRC, or no longer fitting).
- **Write head.** `head_slot` is the ring position being written and `head_offset_bytes` its byte offset in the ring file (`null` before the first write). `wrap_count` counts complete passes over the ring.
- `fs_free_bytes` and `fs_total_bytes` describe the buffer's filesystem, and `min_free_bytes` is the floor enforced on it; all three are `null` when disabled or unknown.
- `paused` is true while writing waits for free space. `failed_samples` counts samples lost to failed writes and back-off. `sync_errors` counts failed ring fsyncs, and `poisoned_samples` the samples written but discarded because their fsync failed.
- `allocation_skipped_samples` (T-217) counts samples captured while the ring was still `"allocating"` (no writer existed yet, so nothing could be buffered); it stops growing once the ring opens and keeps its value from then on, explaining a gap at the start of a large ring's history.
- `t0`/`t1` are the earliest retained sample and the latest end over all runs (`null` when empty). `bytes` = `2 × samples` retained. `disk_bytes` is the buffer's files: `allocated_bytes` plus the journal. `evicted` counts this run's evictions, and `evicted.chunks` the slots overwritten.
- `t0`/`t1` query parameters list only segments overlapping `[t0, t1)`; of the matching segments the newest `limit` are listed, oldest first, and `segments_omitted` counts the rest. `segments_total` counts every retained segment.
- **`Segment`**: `{id, run, t0, t1, t0_ns, t1_ns, samples, global_index, center_hz, sample_rate_hz, bandwidth_hz, lna_db, vga_db, amp_on, device_id, antenna_port, bias_tee, overload, content_class, dropped_before}`.
  - `id` increases for the life of the ring.
  - `bias_tee` (T-325) is the antenna-port bias-tee state under this segment's provenance: `"unknown"`, `"off"` or `"on"`. It sits beside `antenna_port` and `overload` as device-local trust context — an active antenna's LNA changes the noise floor and gain structure, so segments captured with it on are not comparable with ones captured with it off. **`"unknown"` means the source could not report it** (a replayed recording carries no such field) and is never to be read as `"off"`. Switching the bias tee is a provenance change, so it always starts a new segment.
  - `global_index` is the stream index of the first retained sample, in its `run`'s numbering.
  - `dropped_before` counts the samples this buffer lost to overruns just before the segment.
- **`Gap`** (between consecutive listed segments of the same run with missing stream indices or losses): `{t0, t1, before_segment, samples, dropped_samples}`. `samples` counts every missing index (source gaps, settle skips after a retune, overruns, gated blocks); `dropped_samples` those lost by this buffer.

**Clip export.** The body gives exactly one range on the sample clock:
- **`{t0_ns, t1_ns}`**, integer Unix ns: exactly the samples whose time lies in `[t0_ns, t1_ns)`. Sample `k` of a segment is at `t0_ns + round_half_up(k × 10⁹ / rate)`, computed in integers.
- **`{global_index, samples}`**: exactly those stream indices. Use this form from JavaScript, whose numbers cannot hold Unix ns exactly.
- **`{t0, t1}`**, Unix seconds: each is converted once to integer ns by `round(s × 10⁹)`. Near today's epoch an f64 resolves only ≈ 240 ns (about 5 samples at 20 Msps), so a seconds range may start or end a sample off.

Times must be ≥ 0. A clip is sized before anything is written. It is refused if it is over `max_clip_bytes`, or if it would leave less than the free-space floor free on the recordings filesystem.

**`run`** (optional integer) selects only segments of that run.
- **Without it**, an index range selects in the **current run** (indices restart every run), and a time range selects in whichever single run it matches.
- **A time range matching segments of more than one run** answers `409 conflict`: it spans a restart, or a replayed recording repeated its times. Clips are never spliced across runs, so give `run`, or export each side.
- **A clip of an earlier run** is the same bytes it was before the restart.

The ring can overwrite data while it is being exported. The clip then fails with `404 not_found` instead of exporting other data.

With `band`, only segments whose tuned window (`center ± rate/2`) overlaps `[f_lo, f_hi]` are exported; the data stays the window as captured (not channelised) and the band is a SigMF annotation. The clip is written to `recordings/<id>.sigmf-data` (ci8) and `.sigmf-meta`, with a `Recording` row (kind `iq-snippet`, trigger `manual`, retention `pinned`), like a manual recording.
- **SigMF metadata.** `global`: `core:sample_rate`, `core:datatype` `ci8`, `core:hw`, `core:recorder` `hk-pipeline:iqbuffer`, `hackriff:provenance` of the first piece. One `captures` entry per contiguous piece (a buffer segment inside the range): `core:sample_start` in the clip, `core:frequency`, `core:datetime` (sample clock), `core:global_index` (the stream index, so a gap between pieces is explicit, never spliced), `hackriff:provenance` (tuning, gains, filter, antenna, overload), `hackriff:buffer_segment` and `hackriff:buffer_run`.
- **`Clip`**: `{id, label, meta_uri, data_uri, meta_path, data_path, t0, t1, t0_ns, t1_ns, samples, bytes, sample_rate_hz, center_hz, band ([f_lo, f_hi] or null), content_class, captures: [{sample_start, samples, global_index, t0, t0_ns, segment, run, center_hz, sample_rate_hz, bandwidth_hz, lna_db, vga_db, amp_on, device_id}]}` (`*_uri` relative to the data directory).
- **Errors** `{"error", "code"}`: `400 invalid` (no range or more than one, a negative time, start ≥ end, zero `samples`, bad `band`/`label`/`run`, unknown field or query parameter, a clip over `max_clip_bytes`), `404 not_found` (nothing buffered in the range, band and run, e.g. already evicted, or overwritten during the export), `409 conflict` (the range spans a sample-rate change: SigMF has one rate per file, so export each side; or a time range spans runs), `503 unavailable` (the run has no buffer, or this server has none), `507 insufficient_storage` (the clip does not fit above the free-space floor), `500 failed` (storage), `405` other methods. Messages never echo values.

## Persisted IQ recordings (T-469)

**The other half of the audio horizon.** Raw IQ — and so demodulation, decode and audio on playback — exists in exactly two places: the rolling IQ ring (minutes; `GET /api/iqbuffer` above answers its extent exactly) and **persisted SigMF recordings** under `<data dir>/recordings/`, written by the manual recorder (`POST /api/outputs/record/start`), the record chain and the ring's clip export. `GET /api/captures` serves **decoded** captures, which is a different thing. Nothing enumerated the recordings, so "where can I hear audio" was answerable for the ring alone and a playhead built on it would silently under-report the horizon over every recording beyond the ring. CLAUDE.md's playback invariant requires IQ availability to be *its own visible, predictable boundary on the time axis*, and a client cannot draw a boundary for files it cannot enumerate.

Code: `hk_store::recordings` (the query and the on-disk check), `crates/hk-api/src/recordings.rs` (the route). It is a **read**, not an index: a query over the immutable `Recording` rows (`docs/07` §2.12) on their existing `t_start` index, joined to their `Provenance`. Nothing is maintained beside the rows — a catalogue kept next to them could only go stale.

| Method | Path | Query | Response |
|---|---|---|---|
| GET | `/api/recordings` | `?[t0=<unix s>][&t1=<unix s>][&kind=iq-snippet\|channel-decimated\|audio][&limit=1..1000, default 200]` | `{recordings: [Recording], count, matched, omitted, iq_available}` |

- `t0`/`t1` select recordings **overlapping** `[t0, t1)`: a recording ending exactly at `t0` does not overlap. Both are optional and independent. Each is converted once to integer ns by `round(s × 10⁹)`; near today's epoch an f64 resolves only ≈ 240 ns, so a boundary given in seconds is not exact to the sample (the same caveat as the clip route's `{t0, t1}` form).
- `kind` filters the query itself, so `matched` narrows with it. `limit` pages the **newest first**; `matched` counts every recording that matched and `omitted` = `matched − count` those the page left out.

**The row is a claim; the file is the fact.** A `Recording` row is written *after* its samples are and is immutable, so it keeps claiming what it claimed when the file is later truncated, evicted or moved to another data directory. Each listed recording is therefore checked against the filesystem once, at list time, and `state` is what is on disk **now**:

| `state` | Meaning | `available` |
|---|---|---|
| `complete` | Both files present and the data file is **exactly** `size_bytes` | `true` |
| `partial` | Present but not what the row describes: a short data file (partially written or truncated), one longer than the row records, or a missing `.sigmf-meta` without which the samples cannot be read as SigMF | `false` |
| `missing` | No data file at all, or a `*_uri` that is not a path inside the data directory | `false` |

A partially written or missing recording **is listed** — hiding it would be its own dishonesty — but never as available and never in `iq_available.spans`. `detail` says why in words when `state` is not `complete`.

**What this does not cover.** A recording still being written has **no row yet** (the recorder inserts one when it stops), so it cannot appear here at all; while it is in progress the ring covers the same samples and `GET /api/control/state` reports the recorder. The check is existence and length, not a CRC over the samples.

**`Recording`**: `{id, kind, iq, t0, t1, t0_ns, t1_ns, duration_s, center_hz, sample_rate_hz, f_lo, f_hi, pre_trigger_s, post_trigger_s, trigger, retention_class, content_class, meta_uri, data_uri, size_bytes, state, available, bytes_on_disk, meta_present, detail, device_id, antenna_port, bias_tee, bandwidth_hz, lna_db, vga_db, amp_on, overload}`.
- `kind` is `iq-snippet`, `channel-decimated` or `audio`; **`iq`** is true for the first two — what can be re-demodulated and re-decoded on playback. Audio cannot, so it never extends the demod/decode horizon.
- `f_lo`/`f_hi` are the tuned window as captured, `center_hz ± sample_rate_hz/2` — the same convention the IQ ring's segments and its clip band filter use.
- `meta_uri`/`data_uri` are **relative to the data directory**, exactly as written, so the pipeline (and a `SigmfReplaySource`) can open them.
- `size_bytes` is what the row records; `bytes_on_disk` is what the data file holds now (`null` when there is none).
- `trigger` is `{"kind": "detection" \| "demodulation" \| "scheduler" \| "manual", "id"?}`; `retention_class` is the C25 eviction class (`pinned`/`unknown`/`decoder-confirmed`/`routine`).
- `device_id`, `antenna_port`, `bias_tee`, `bandwidth_hz`, `lna_db`, `vga_db`, `amp_on` and `overload` come from the recording's `Provenance` — which front end captured it and under what state. All are `null` when that provenance row cannot be read; none is ever guessed. `bias_tee` follows the same three-valued rule as the ring's segments: `"unknown"` means the source could not report it and is never to be read as `"off"`.

**`iq_available`**: `{horizon: "iq-ring + recordings", ring, spans: [Span]}` — the whole answer to "where is raw IQ still readable", **ring plus these recordings** rather than the ring alone.
- **`ring`**: `{enabled, reason, t0, t1}`, the ring's own window read from the same status `GET /api/iqbuffer` serves (`t0`/`t1` `null` when it holds nothing; `enabled: false` with a `reason` when there is no ring).
- **`Span`**: `{t0, t1, t0_ns, t1_ns, span_s, source: "recording" \| "ring", recording}` — `recording` is the recording's id for `source: "recording"`, `null` for the ring. Oldest first, on the one shared time axis whatever the source.
- Only **`state: "complete"` recordings with `iq: true`** contribute a span.
- **The spans are deliberately not merged into one `t0`/`t1` envelope.** IQ availability has holes, and an envelope over a hole promises audio that does not exist — the same defect as implying resolution that was never captured. The client unions the spans it is given and draws the gaps.
- The spans cover **exactly the page listed**, so a truncated page (`omitted > 0`) carries a truncated horizon; widen `limit` or the window to see more. The ring's span is always present.

- **Errors** `{"error", "code"}`: `400 invalid` (unknown query parameter, a non-numeric or negative `t0`/`t1`, `t0 ≥ t1`, an unknown `kind`, a `limit` outside 1..1000), `503 unavailable` (this server has no recording catalogue — no data directory or database), `500 failed` (storage), `405` other methods.

## Classification taxonomy (T-218, ADR-0016 §1–§2)

| Method | Path | Body / query | Response |
|---|---|---|---|
| GET | `/api/taxonomy` | – | `{"current", "unknown", "taxonomies": [...], "thresholds": {...}, "coarse": [...]}` |

The modulation taxonomy and the decision thresholds **as data**, so the thin client never keeps its own copy of the family tree, the label spellings or the SNR gates (a stale copy would tell a different story from the one the backend decided). Code: `hk_model::classify::{taxonomy, thresholds}`, served by `crates/hk-api/src/taxonomy.rs`.

- **`current`** is the taxonomy new classifications are written under (`"hk-mod@1"`); **`unknown`** is the open-set label (`"unknown"`), which is an outcome at every level and never a leaf of the tree.
- **`taxonomies`**: every *released* version, oldest first — a stored row keeps the version it was written under, so a reader maps its labels with the matching entry. Each is `{"ref", "name", "version", "families": [{"family", "coarse", "classes": [...]}], "legacy": [{"label", "family"}]}`. `coarse` is `analog` / `digital` / `noise-like`. `legacy` maps pre-taxonomy spellings (e.g. `fsk2` → `fsk`); labels that are already a family or class name are not repeated there. Service labels (`adsb`, `fm-broadcast`, decoder ids) are **not** modulation labels and appear nowhere here.
- **`thresholds`**: `{"version": "thresholds@1", "max_confidence", "lambda0_min", "families": [{"family", "snr_gate_db", "class_gate_db", "min_confidence", "open_set_max"}]}`. `snr_gate_db` is `null` for a family with no SNR gate (`noise-like` is a shape test). Below its gate a family contributes no likelihood mass — its share moves to `unknown` with reason `low_snr`, because "not measured" is not "ruled out". `max_confidence` (0.999) is the cap that keeps any call from being reported as certain; `lambda0_min` (0.1) is the smallest uniform weight a C17 prior may carry, which is what stops a band-plan prior from driving a family to zero.
- **Reference data, not measurement.** No emitter, detection or identity is reachable through this route, and it pre-populates nothing: what was actually *found* comes from `/api/inventory`, always from blind detection first. Errors: `405` (other methods), `401` (token).

## Signature matches (T-201, ADR-0016 §5)

| Method | Path | Body / query | Response |
|---|---|---|---|
| GET | `/api/signatures/match` | `?emitter=<id>` | `{"emitter", "match": SignatureMatch \| null, "history": [SignatureMatch, ...]}` |

What the **editable signature catalogue** has to say about one emitter's measured parameters. Code: `hk_model::signature` (the types and their storage), `hk_context::signature` (feature aggregation and the matcher), `crates/hk-api/src/signatures.rs` (this route).

- **Evidence, never identity.** A match sets no identity, no `known_status`, no lifecycle state and no family — nothing on the emitter changes because of it. It is a ranked, reasoned suggestion beside the measurement, exactly like a band-plan prior, and a client must present it that way. Only a CRC-valid decode confirms a signal. The catalogue is never the starting point and never overrides what was measured (the exploration-first rule).
- **`SignatureMatch`**: `{schema, emitter_id, t_ns, outcome, features_ref, signatures_rev, candidates: [...], reasons: [...]}`. `t_ns` is when the match was computed, Unix nanoseconds.
  - **`outcome`** is `full`, `partial` or `none`. **`none` means the catalogue has nothing to say — not that the emission is unknown.**
  - **`candidates`** (≤ 5, best first, empty on `none`): `{signature: {id, version}, name, score, agreement: [{field, measured, expected, z, ok}], missing: [...], conflicting: [...], recipe: {id, version}?}`. `z` is a normalised distance, so `z ≤ 1` (`ok`) is agreement whatever the field's units and `z > 3` is an active conflict. `missing` names the required fields not measured yet — what a decoder search should go and estimate next; `conflicting` names the fields that actively disagree.
  - **`score`** (0–1) is `Σ w·exp(−z²/2) / Σ w_required`: the weighted agreement of every compared field over the weight of every *required* field. Missing required fields contribute nothing to the numerator while still counting in the denominator, so a measurement with few fields scores low by construction. It ranks candidates against each other on one measurement; it is not a probability that the emission *is* that protocol.
  - **`reasons`** are machine codes: `too_few_fields`, `missing_required`, `ambiguous_candidates`, `conflicting_required`, `all_suspect`, `no_candidate`.
  - **`features_ref`** names the `EmissionFeatures` snapshot it was computed from and **`signatures_rev`** the catalogue revision, so any match can be re-derived exactly.
- **Too few fields never identify anything.** A `full` match needs every required field measured and agreeing, at least three of them (a floor the matcher enforces regardless of what a catalogue entry declares, since imports are untrusted), and a score ≥ 0.8. Anything less is a ranked `partial`. When two entries both fit completely — the P25/DMR near-collision, where both run at 4800 Bd and differ only in deviation and sync word — neither wins: the outcome is `partial` with both ranked and the reason `ambiguous_candidates`.
- **Mismatches are interesting.** An emission whose parameters sit beside a known protocol's without fitting it stays a `partial` with the conflicting fields named, never snapped to the nearest entry. Bands are rank-only and gate nothing, so an emission in the "wrong" band still matches and is visibly off-band; a different modulation family does rule an entry out, while an `unknown` family rules nothing out.
- **`history`** is the append-only match log, newest first (≤ 50), so what the catalogue said about this emitter over time is kept rather than overwritten. A new row is appended only when the outcome or top candidate changes.
- **Gating.** Like `/api/inventory/{id}/decode`, an emitter whose decoded identity is withheld answers exactly as one with no matches — no flag, no count, no marker — so a match listing can never confirm a withheld identity. An emitter with no decoded identity is served normally.
- **Errors** `{"error", "code"}`: `400 invalid` (no `emitter`), `404 not_found` (unknown or unparsable id), `503 unavailable` (this server has no inventory), `500 failed` (storage), `405` other methods.

## Clusters of unknown emissions (T-202, ADR-0016 §5)

| Method | Path | Body / query | Response |
|---|---|---|---|
| GET | `/api/clusters` | – | `{"clusters": [Cluster, ...]}` (visible only, oldest first) |
| GET | `/api/clusters/{id}` | – | one `Cluster`, with `member_ids` and `events` |
| POST | `/api/clusters/{id}/promote` | `{}` | `{"signature": {"id", "version", "name"}, "cluster": Cluster}` (audited `cluster_promote`) |

*"The same thing I saw before."* Emissions nothing identifies are grouped by **how they measure**, so an unknown burst can be shown as the fourth sighting of something already catalogued as unknown. Code: `hk_model::signature::cluster` (types and storage), `hk_context::signature::cluster` (the distance, online assignment and the repair pass), `crates/hk-api/src/clusters.rs` (these routes).

- **Evidence, never identity.** A cluster sets nothing on any emitter — not identity, not family, not `known_status`, not lifecycle. It is a *type* above emitters, which are *instances*: two identical sensors share a cluster and stay two inventory rows. Only a CRC-valid decode confirms what something is.
- **`{id}`** may be written `cluster:0199…` or just `0199…`; the prefix is optional so a path never has to carry an encoded colon. An id that has been merged away resolves to its survivor.
- **`Cluster`**: `{id, state, members, member_ids: [...], created_at_s, updated_at_s, observations, suspect_fraction, feature_set_version, merged_into, signature: {id, version} | null, centroid: [...], events: [...]}`.
  - **`state`** is `pending`, `active`, `merged` or `promoted`. **Only visible clusters are served** (`active` and `promoted`): a group becomes visible at 3 member emitters, or 1 emitter seen in 3 separated appearances. A `pending` group answers `404` — it is still a guess, and a guess with an id reads as a finding.
  - **`centroid`** is what the group measures like, with its uncertainty disclosed: `[{field, kind (`num`/`bits`/`text`), value, sigma, spread, agreement, n, method}]`. `sigma` is `max(spread, sigma_meas)` and **never shrinks as 1/√n** — a drifting oscillator is not better known for having been looked at more often.
  - **Which fields are compared:** symbol rate, deviation, levels, line code, preamble, sync word, packet length, CRC, period, duty cycle, burst length, TDMA period, hop raster and count, OBW, spectral shape, comb spacing/count, radar PRI/scan, family and class. **Frequency is not** (`f_center_hz`, `f_lo/hi`, raster offset), and neither are `snr_db` or `cfo_offset_hz`: a type is not a frequency, and propagation and an individual crystal describe this receiver and this transmitter, not the protocol. Per-transmitter RF fingerprinting is deliberately not done (AWARE-047/051).
  - **`events`** (≤ 50, newest first) is the append-only history: `{kind (`created`/`activated`/`merge`/`split`/`reassign`/`promoted`), other_cluster_id, t_s, detail}`.
- **How a sighting joins.** The distance is the same tolerance-normalised `z` the signature matcher uses, RMS over the fields both measured, with **both** measurements' uncertainties widening every tolerance in quadrature. A new emitter joins the nearest cluster within `z_rms ≤ 1`, seeds one, or is left unassigned. A join is refused — unconditionally — when any single shared field actively disagrees (`z > 3`), whatever else the two share and however few fields that is; when fewer than 3 fields are comparable at all; or when the RMS exceeds 1. Between two clusters that are not themselves compatible the clusterer **abstains** rather than guess: two genuinely distinct emitters sharing an id is worse than two ids for one emitter. A batch DBSCAN repair (ε = 1, minPts = 3) re-derives the partition on demand and records merges and splits; the larger side's id survives.
- **Promotion** mints a `Signature` with provenance `cluster-promoted` from the centroid, with tolerances no tighter than the members actually showed. It is still ranked evidence the matcher scores like any other entry, and it names nothing. Refused (`400 invalid`) when the cluster is already promoted, is not visible, has only suspect observations behind it (the front end manufactures ghosts with real-looking parameters), or measured fewer than 3 discriminating fields.
- **Gating.** Members whose decoded identity is withheld are absent from `member_ids` **and** from `members` — no flag, no count, no marker — so a member list can never confirm a withheld identity. The same rule puts `cluster_id: null` on a withheld inventory row.
- **Errors** `{"error", "code"}`: `404 not_found` (unknown, unparsable or not-yet-visible cluster), `400 invalid` (unpromotable cluster, unknown body field), `503 unavailable` (no inventory, or no audit log for the mutating route), `405` other methods.

## Labelled-capture dataset export (T-205, ADR-0016 §7/§9)

The path from live captures to a training/evaluation set: normalised IQ snippets, each with a `hk-mod@1` label and provenance, for later model fine-tuning (C38) and blind evaluation (T-213). Code: `hk_store::dataset` (label-finding and the manifest shape), `crates/hk-api/src/datasets.rs` (routes).

- **Label sources**, both marked in the exported annotation's provenance:
  - **decoder-validated**: a decode whose CRC check passed (`crc_status: "valid"` only — never a future bounded-correction status, ADR-0016 §7: "corrected frames do not count"). The label is the demodulation's `mode` mapped into the current `hk-mod@1` taxonomy (the same mapping pre-M3 labels use, e.g. `2fsk` → family `fsk`, `wfm` → family `analog`); a decode whose mode does not map into the taxonomy (a service-only label) contributes no dataset sample.
  - **user**: an emitter's current classification when it was set by an explicit user reclassification (arbitration rank `user`), carrying its own `hk-mod@1` family/class and confidence. An `unknown` call is not exported as a positive label.
- **What each sample carries**: the IQ snippet (an ordinary SigMF `Recording`, `kind: iq-snippet`, written by the same clip exporter as `/api/iqbuffer/clip`), its sample rate and tuned centre, the labelled emission's centre offset from that tuned centre, a measured SNR when known ([`Repository::emitter_latest_measurement`]), the `hk-mod@1` label with taxonomy version, the source emitter and session (the producing demodulation's id for a decoder label; `null` for a user label — ADR-0016 §7 splits by session, never by frame), the label event's timestamp, and the label source (`decoder`/`user`).
- **Provenance and label storage.** Each sample's label is a `GroundTruth` (decoder-sourced) or `Label` (user-sourced) `Annotation` on the Recording (docs/07 §2.13), `author` `decoder`/`user`, `metadata` carrying `{taxonomy, label, source, split, emitter_id, session, snr_db}`, `exported: true` from the moment it is written. Nothing here overwrites or re-labels an existing annotation.
- **Format.** SigMF recordings (reusing the existing IQ-capture-buffer clip writer — no new I/O code) plus a JSON **manifest** that indexes the resulting recordings and stamps one split on the whole export. No database migration was needed: the manifest is a file (like a `.sigmf-meta`), and the labelled data itself is ordinary `Recording`/`Annotation` rows the repository already had.
- **Dev/acceptance split (ADR-0016 §7).** Every export names its `split` (`"dev"` or `"acceptance"`) once, in the request; every sample and the manifest itself carry it. There is no automatic split inference (e.g. from time or content) — a training pipeline reads `split` and never mixes them, so acceptance data (T-206) can never contaminate training.
- **Snippet availability.** A labelled emission whose snippet the underlying buffer can no longer produce (evicted, out of retention) is counted in the manifest's `skipped`, not treated as a failure of the whole export.

| Method | Path | Body / query | Response |
|---|---|---|---|
| POST | `/api/datasets` | `{"filter": {"emitter"?, "time"? {"t0", "t1"}, "family"?}, "split": "dev" \| "acceptance", "pad_pre_s"?, "pad_post_s"?, "max_samples"?}` | `{"dataset": DatasetManifest}` (201; audited `dataset_export`) |
| GET | `/api/datasets` | – | `{"datasets": [DatasetManifest, ...]}`, newest first |
| GET | `/api/datasets/{id}` | – | `{"dataset": DatasetManifest}`; 404 `not_found` |

- **`filter`** (all set fields must hold; omit for every candidate emitter): `emitter` (an id, overrides the others), `time` (`{t0, t1}` Unix s, an emitter's last-seen window), `family` (an `hk-mod@1` family name — matched per labelled emission, not the emitter's current classification, since a decoder label need not have one recorded).
- **`pad_pre_s`/`pad_post_s`** (default 0.05 s each): snippet padding before/after the label event. **`max_samples`** (default 200): a guard on both the search and the export.
- **`DatasetManifest`**: `{id, filter, split, created_at, samples: [DatasetSample], skipped}`.
- **`DatasetSample`**: `{emitter_id, session, recording_id, annotation_id, label: {taxonomy, label, source, provenance, confidence}, snr_db, sample_rate_hz, center_offset_hz, t, split}`.
- **Errors** `{"error", "code"}`: `400 invalid` (bad filter/split/padding/`max_samples`, unknown field), `404 not_found` (no such manifest), `503 unavailable` (this server has no dataset export, e.g. no IQ capture buffer), `500 failed` (storage), `405` other methods.

## Streams: WebSocket, TCP and on-demand openers

Full framing, header fields, binary record layout, drop markers, backpressure and `content_class` egress gating are the versioned wire contract: **[`docs/stream-contract.md`](stream-contract.md)**. This section covers only the HTTP/WS-level *endpoints* that open or discover a stream.

| Method | Path | Auth | Response |
|---|---|---|---|
| GET | `/ws/{stream_id}` | token (header or `?token=`) | Upgrades to WebSocket and bridges the named always-on stream (§10) |
| GET | `/ws/open/{name}` | token | Upgrades and opens an on-demand stream (§12): `listen`, `bits`, `symbols`, `iq`, with query parameters per opener |

**`GET /ws/{stream_id}`** (e.g. `spectrum/live`): the header JSON is the first **text** message, verbatim; every later record is one message — text (NDJSON line) for `messages` streams, binary (32-byte record header + payload) for every binary kind. Refusals never upgrade the connection and are plain HTTP: `401` (bad/missing token, checked before the upgrade), `403` (a `own-key-decrypted` stream — those are Unix-socket-only and never served over the bridge), `404` (unknown `stream_id`), `410` (stream finished), `426` (not a valid WebSocket upgrade request), `503` (consumer cap reached, or `replumbing` — see below).

**A retune does not disconnect you (T-417).** A `stream_id` outlives its publishers: a retune finishes the spectrum publisher and offers a new one under the same id (a header must describe every row after it, T-057), and a re-plumb rebuilds every reader around the still-open device. The bridge **carries the connection across**: the socket stays open, and the next publisher's header arrives as **another text message** on the live connection, with the records after it belonging to that new header. A client reads any later text message whose `"schema"` is `"hackriff.stream"` as a new header (no record carries `schema`). Between the two there is a **real gap in the data** — the front end was moving — and nothing is sent to cover it: no held frame, no repeated row, no interpolation. The header is the seam. Nothing of the new window is swallowed either (T-425): the re-subscribe waits on the next offer rather than on a timer, so the first record after the new header is that window's first record, and the gap you measure is the front end's, not the bridge's. If no new publisher is offered within 60 s the connection ends as before. **This is not the slow-consumer policy** (§7): a consumer that cannot keep up is still dropped deliberately, and so are `PeerGone`, `DrainTimeout` and `Detached` — only "the producer replaced this stream's publisher" is carried. The framed **TCP/Unix** transport is unchanged: a header there is still once per connection, and a consumer of an always-on stream over TCP is disconnected when its publisher finishes.

**Arriving *during* a re-plumb is not an end either (T-530).** The paragraph above is about a connection that is already attached; a client that opens one while the front end is between windows used to be answered `410 Gone, "stream finished"` — a run that had not finished at all, described as permanently over. A registered `stream_id` whose publisher has finished is **between windows**, not gone, so the handshake now waits up to **2 s** for the producer's successor and upgrades on it (the measured gap is ~0.17 s, so this is normally invisible: you get `101` and the new window's header). If the successor still has not been offered when that runs out, the refusal is **`503`** with `Retry-After: 1` and `{"code": "replumbing"}` — *not now*, never *never again* — and the same `503 replumbing` is what `/ws/open/{name}` and the TCP transport answer in that state. **`410` still means the stream is really over**: the producer says which end it is, and a run that has ended (its source finished, or it was stopped) answers `410` at once, with `run.capture: "ended"` on `/api/control/state` saying why (T-508). Health checks should treat `503` as "the server is alive and busy", not as a failure.

**`spectrum/live` is computed only while somebody is reading it (T-489).** The stream is always *offered* — it appears on `/api/streams` and accepts a subscriber whenever the run is live — but the producer's FFT runs only while at least one consumer is open, so a headless run (a scheduled survey with no browser attached) publishes **no rows at all** and the run's spectrum row counter honestly reads `0`. A **consumer** is what counts: this WebSocket, a TCP stream client, or an in-process subscriber. Nothing else changes — capture, the IQ ring, detection, the spectrum-history pyramid and the coverage map each read the ring themselves and are identical either way, which is the point: the data must not change because nobody was looking. On subscribing, rows start within one producer read timeout plus a row period (measured ~0.1 s), and the first one carries `DISCONTINUITY`, because it is not contiguous with whatever row was published before the quiet stretch.

**`GET /ws/open/{name}?<params>`**: e.g. `listen?emitter=<id>` or `listen?f_lo=<Hz>&f_hi=<Hz>` (mode and parameters are always estimated — there is no `mode` parameter), `bits`/`symbols` (optionally `emitter=`/`detection=`/`f_lo=&f_hi=`), `iq?emitter=<id>` or `iq?f_lo=<Hz>&f_hi=<Hz>` (T-165, ADR-0013 §4.9 gap 8: raw channelised IQ, `cf32_le`, stream-contract §12.3 — no mode or parameter either, there is nothing to demodulate). Unlike `/ws/{id}`, a **refusal completes the upgrade** (browsers cannot read an HTTP error body on a failed upgrade): one text message `{"type": "refused", "status", "code", "reason", "content_class"?}`, then the socket closes with code `4000 + status` (e.g. `4403` a legal/class refusal, `4404` unknown opener/target, `4409` outside the tuned window or mid-replumb, `4503` at the listener/chain/CPU budget). A refusal never carries content. On success the connection is bridged exactly like `/ws/{stream_id}` above (header text message, then records) as a **remote** consumer, so an `own-key-decrypted` target is refused the same way. **Listen** additionally streams periodic **status** records (binary, type 3: `level_dbfs`, `snr_db`, `squelch_open`, `agc_gain_db`, `frames`, `latency_ms`, …).

### `hk` stream-tail

`hk stream-tail --uds <path> | --tcp <addr> [--count N]` (local process, not an HTTP route) prints a stream's header as pretty JSON, then one line per record — useful for eyeballing any of the above without a browser.

### TCP stream server (T-060)

For external programs (netcat, socat, a Python script, GNU Radio) that can't or shouldn't speak WebSocket. `hk serve` binds it to `127.0.0.1:8788` by default (`$HK_STREAM_TCP` to change; an ephemeral loopback port if 8788 is taken); its address is reported by `GET /api/streams` (`"tcp"`) and printed at start.

**Handshake:** after connecting, send **one line** (≤ 4096 bytes, `\n`-terminated) within 10 s:

```text
<stream_id>?token=<token>              an always-on stream, e.g. spectrum/live
open/<name>?token=<token>[&k=v...]     an on-demand opener, e.g. open/bits, open/listen?emitter=<id>
```

The server then sends the §3 framed byte stream exactly as on a Unix socket (the header frame, then records) — **or, instead of the header, one refusal frame** whose JSON body is `{"type":"refused","status","code","reason","content_class"?}`, after which the connection closes. A reader tells a header from a refusal by its `schema` (header) vs `type` (refusal) field. Statuses: `400` bad handshake, `401` token, `403` local-only/legal refusal, `404` unknown stream/opener, `408` handshake timeout, `410` finished, `431` line too long, `503` at capacity. **Nothing about a stream is revealed before the token verifies** — a wrong token gets the same `401` whether or not the target exists.

Consumers never send after the handshake line; any byte, or a hang-up, closes the consumer (and, for an on-demand stream, stops its producer). One-liner (hex dump of every demodulated burst's bits):

```sh
printf 'open/bits?token=%s\n' "$HK_TOKEN" | nc 127.0.0.1 8788 | xxd | head -40
```

Python clients (standard library only): `py/examples/` (`hkstream.py`, `hk_bits.py`, `hk_audio_wav.py`), documented in `py/README.md`.

### `open/iq` — on-demand channelised IQ (T-165, ADR-0013 §4.9 gap 8)

The UI's inventory row "Stream out" action, and any external tool (GNU Radio, a Python script) that wants a signal's own raw samples rather than a demodulation. `GET /ws/open/iq?<params>` / TCP `open/iq?<params>&token=…`, modelled on `listen`/`bits`/`symbols` (§12.1) but with nothing estimated or demodulated:

- **Target**: `emitter=<id>` (the inventory entry's measured centre and bandwidth) or `f_lo=<Hz>&f_hi=<Hz>` (an explicit band). Exactly one; any other parameter (`mode`, `detection`, …) is refused `400` — there is nothing to estimate.
- **Profile** (stream-contract §12.3): `kind: "iq"`, `datatype: "cf32_le"` (`re, im` `f32` LE pairs), `sample_rate_hz` the channel DDC's own output rate, `center_hz`/`bandwidth_hz` the requested band, `emitter_id` when the target was an emitter. One binary data record per processed chunk; a gap (a retune skip, a live-source backlog skip) is flagged `DISCONTINUITY`.
- **Bounds.** The requested band must be at most 2 MHz wide (`hk_stream::iq::MAX_IQ_SPAN_HZ`) and inside the tuned window. The channel runs on its own thread reading the shared ring at its own pace (exactly like a Listen chain), computed **only while a consumer is attached** — no chain, no DDC, until the socket opens, and it never blocks or slows capture: a live source skips forward instead of growing a backlog, and a slow consumer is dropped by the publisher (§7), never the pipeline. It shares the run's on-demand chain budget (T-071, `/api/status` `budget`) as a burst-tap-kind chain, costed like a Listen chain from the tuned sample rate (the DDC's input-rate filter stage, not how much the channel itself decimates).
- **Gating.** Raw IQ is content — more directly than demodulated audio, since it carries the RF envelope besides — so it is gated exactly like `bits`/`listen`: the legal gate (`hk_pipeline::chains::listen::listen_class`, the same rule Listen and burst content already use) runs before any ring read, and the stream's `kind: "iq"` is one of the profile's content-bearing kinds, so the egress gate withholds the payload (`GATED`, header-only) on any stream whose class forbids it regardless.
- **Refusals**: `400` bad request (unknown parameter, no `emitter`/`f_lo`+`f_hi`, span over 2 MHz), `403` legal gate, `404` unknown emitter, `409 outside-window` (the band is not inside the tuned window), `410`/`503` segment ended/replumbing, `422 unrealisable` (the channeliser cannot down-convert the band at the tuned rate), `503 busy` (chain budget).

```sh
printf 'open/iq?f_lo=%s&f_hi=%s&token=%s\n' 101190000 101410000 "$HK_TOKEN" | nc 127.0.0.1 8788 > station.cf32
```

## Inspector (T-089, SIGNAL-062)

The declarative parser's routes (ADR-0011 §3–4). They evaluate a **draft** field map (a `hk_recipe::FieldMap` JSON document, [ADR-0011 §3.1](adr/0011-decoder-workbench-contracts.md)) over frames or over a recorded decoded stream **without saving anything**, so the parser-authoring loop is: edit the map, re-parse the whole recording, read the fit summary, repeat. All parsing is server-side; the UI renders what comes back.

| Method | Path | Auth | Body → response |
|---|---|---|---|
| POST | `/api/inspector/parse` | token (header) | `{field_map, frames: [{hex, bit_len?}]}` (1–500 frames) → `{frames: [{bit_len, hex, layers}], fit}` |
| POST | `/api/captures/{id}/parse` | token (header) | `{field_map?, from_frame?, limit?}` → `{capture_id, stream, total_frames, from_frame, limit, next_from_frame, frames, fit}` |

**Layer trees** (`layers`) are the `docs/stream-contract.md` §14.2 shape: `nodes` in pre-order, each with `id`, `parent`, `name`, `path`, `type`, `bits: [offset, length]` (absolute from the frame's first bit, MSB of byte 0 first), `bytes: [first, end)`, `value`, `text` (rendered, with `value_unit`), `label`, `error`; `byte_index[b]` lists the leaf nodes overlapping byte `b` in bit order; `fit` is `ok`/`partial`/`failed`; `errors: [{path, kind, need_bits?, have_bits?}]` with `kind` `out-of-bounds`, `bad-length`, `missing-reference`, `repeat-limit`, `node-limit` or `parity`. **Linked selection uses only these:** field → highlight `bytes` (or `bits`); byte `b` → select `byte_index[b][0]`, repeat clicks cycling. A field that doesn't fit never aborts the frame.

**`fit` summary:** `{frames, ok, partial, failed, unparsed, errors: {<path with indexes removed, e.g. items[].id>: {<kind>: count}}}`.

**`POST /api/inspector/parse`**: `hex` is the frame's bytes (even-length hex, either case); `bit_len` defaults to 8 × bytes and may not exceed it. `field_map` is required. The response's `hex` is normalised to lower case.

**`POST /api/captures/{id}/parse`**: re-parses a recorded decoded stream (§14.7: the §3 byte stream itself; T-092 records them, `ApiState::captures` / `hk_stream::inspector::CaptureSource` opens them).
- `from_frame` (default 0) and `limit` (1–500, default 100) page over the recording's frame records in stored order; `total_frames` counts them all and `next_from_frame` is the next page's start (`null` on the last page).
- `frames` are the stored `frame` records. With a `field_map`, each parseable record gains `content.layers` and `metadata.fit`; without one, records are returned as stored (the paged frame list with `content.hex`). `metadata.recipe_version`/`edit_rev` stay the recording's.
- `fit` covers **every frame of the recording**, not just the page (`null` without a `field_map`), up to the first 100 000 frames (`MAX_FIT_FRAMES`) so one request's CPU is bounded. It adds `truncated`: `true` when the recording has more frames than that, so the summary counts only the first 100 000 (`total_frames` still counts all of them, and pages past the cap are still parsed).
- `stream`: `{stream_id, content_class, message_schema, inspector?}`; `inspector.source` is `{kind: "capture", capture_id, reparse}`.
- **Gating (fail closed):** a record whose class, or whose stream's class, forbids content, or that is `own-key-decrypted` (local consumers only), is returned with `gated: true` and no `content`, is never parsed, and counts as `unparsed`.

Errors are `{"error", "code"}`: `400 invalid` (bad body; an invalid field map adds `errors: [{path, message}]` with dotted field paths, as `FieldMap::validate` reports them), `404 not_found` (no such capture), `405` (not POST), `413` (body over the 64 KiB cap; answered by the HTTP layer before routing, as `{"error": "Payload Too Large"}` without a `code`), `415 unsupported_media_type`, `422 unreadable` (the capture opened but is not a readable inspector stream: bad header or framing), `500 unreadable` (the capture store failed to open it: a server-side I/O error, not the recording's fault), `503 unavailable` (this server has no capture store; `hk serve` has one since T-092, see "Decoded captures"). Messages never echo values. These routes only read: they need the token in the header like every POST, but are not audited.

## Recipes and pipelines (T-088, M1; ADR-0011 §2.3–§2.5)

A **recipe** is a decoder as data (`hk_recipe::Recipe`, JSON; ADR-0011 §2). A **pipeline** runs one recipe as a chain on the running capture: one ring reader, a channel DDC to `input.sample_rate_hz`, and the recipe's block graph. Pipelines are admitted as `recipe` chains in the run's on-demand chain budget (`/api/status` `budget`), are hot-edited without stopping capture, and serve their outputs over the stream contract (§14). All signal logic is in `hk_pipeline::recipes`; these routes only route, audit and shape errors (`crates/hk-api/src/recipes.rs`).

Conventions:
- **Bodies.** Recipe bodies are the recipe documents themselves (64 KiB body cap). Unknown fields are errors.
- **Validation errors.** `400 invalid` with `{error, code, errors: [{path, message}], warnings}`. Messages never echo values.
- **Storage.** Built-in recipes are `recipes/*.recipe.json` (read-only; `$HK_RECIPES_DIR` overrides the directory). Saved versions are files `<data dir>/recipes/<id>/<version>.json`. A save always writes `latest + 1` (the built-in version counts), and a saved version is immutable.
- **Targets.** A pipeline attaches to what the user names: `{emitter_id}` (an inventory entry's measured centre and bandwidth), `{selection_id}` (a persisted selection's extent) or `{band: {f_lo, f_hi}}`. `{capture_id}` answers 422 until T-092. Recipe `match` hints never tune anything.
- **Class.** A pipeline's streams carry `clamp(source class of the channel, recipe output_policy.content_class)`. Under a class that forbids content, frame bytes and layers are withheld and metadata is reduced to the §14.2 keys the recipe's `metadata_keys` names.
- **Audit.** Mutating routes are audited (`recipe_save`, `recipe_delete`, `pipeline_start`, `pipeline_edit`, `pipeline_save`, `pipeline_stop`, `pipeline_channels`, `pipeline_channels_refresh`) with ids and revisions, not whole documents. `POST /api/recipes/validate` saves nothing: it needs the token in the header like every POST but is not audited.

| Method | Path | Body | Answers |
|---|---|---|---|
| GET | `/api/blocks` | – | `{"blocks": [BlockDescriptor]}`: `name`, `version`, `group`, `doc`, `inputs`/`outputs` (`{name, types, diagnostic}`), `params` (`{name, type, required, default, hot, doc}`), `params_pinned` |
| GET | `/api/recipes` | – | `{"recipes": [{id, name, version (latest), versions, builtin, builtin_version, description, match, input: {port}}]}` by id |
| POST | `/api/recipes` | recipe document | 201 `{id, version, warnings, recipe}`: saved as `latest + 1` after validation against `/api/blocks` |
| POST | `/api/recipes/validate` | recipe document | 200 `{valid, errors, warnings, edges: [{node, port, from ("input" \| "node.port"), type}]}`. Nothing is saved. |
| GET | `/api/recipes/match` | – | `?emitter=<id>`: every recipe ranked against that emitter's **measured** parameters, with per-field reasons ("Recipe matching" below) |
| GET | `/api/recipes/{id}` | – | the latest version's document; 404 `not_found` |
| GET | `/api/recipes/{id}/versions/{version}` | – | that version's document |
| DELETE | `/api/recipes/{id}` | – | `{id, deleted_versions}`: every saved (user) version; 409 `conflict` when only a built-in exists |
| POST | `/api/pipelines` | `{recipe_id, version?, target}` or `{recipe, target}` (an unsaved draft) | 201 the pipeline (below). Refusals: 400 `invalid` (with paths), 404 unknown recipe/target, 409 `outside_window` (the channel is not inside the tuned window), 422 `unrealisable` / `unsupported_input` (non-`iq` input, `follow-hops` until T-093, a capture target until T-092), 503 `busy` (chain budget; running chains untouched), 503 `unavailable` (re-plumbing), 410 `source_ended` |
| GET | `/api/pipelines` | – | `{"pipelines": [pipeline]}` |
| GET | `/api/pipelines/{id}` | – | the pipeline |
| PUT | `/api/pipelines/{id}/recipe` | draft recipe document (same `id`) | 200 `{id, edit_rev, applied_at_sample, plan, swap: {rebuilt, reset, updated, kept}, warnings}`. An invalid draft is 400 and the running revision is untouched; 409 `ended`; 504 `timeout` (no chunk boundary within 10 s; nothing changed) |
| POST | `/api/pipelines/{id}/save` | – | 201 `{id, version, pipeline_id}`: the running revision saved as the recipe's next version |
| PUT | `/api/pipelines/{id}/channels` | `{channels_hz: [Hz, ...]}` | Follow-hops pipelines (T-093/T-107). 200 `{id, channels: [{index, center_hz, bandwidth_hz}], added: [channel], removed: [index], applied_at_sample}` (`applied_at_sample` null when nothing changed). A running channel within a quarter channel bandwidth of a requested one keeps its instance and state; new channels start at a chunk boundary without a gap on the others; missing ones stop. Channel indices are never reused. Refusals: 400 `invalid` (not an array of positive frequencies), 404, 409 `ended`, 409 `outside_window` (a channel is not inside the tuned window, also when a retune lands before the change applies; nothing changes), 422 `not_follow_hops`, 422 `no_channels`, 422 `too_many_channels` (above the recipe's `max_channels`), 503 `busy` (each added channel claims one chain of the budget; nothing changes), 504 `timeout` |
| POST | `/api/pipelines/{id}/channels/refresh` | – | Re-resolves the pipeline's channel source (`list_hz`, the hop-set emitter's measured `hop_set_hz`, or the blind detections in its band) and applies it like `PUT …/channels`; same answer and refusals |
| DELETE | `/api/pipelines/{id}` | – | `{"stopped": pipeline}`: the pipeline stops, its streams finish and `/api/streams` no longer lists them |

**Pipeline** JSON: `id` (`p<n>`), `recipe_id`, `recipe_version`, `edit_rev` (0 = as started), `state` (`running` \| `ended`), `end_reason` (`stopped`, `source-ended`, `segment-ended`, `retune: …`, `rate-change: …`, `error: node <id>: …`), `target`, `channel: {center_hz, bandwidth_hz, sample_rate_hz}`, `content_class`, `emitter_id`, `started` (Unix s), `nodes: [{id, block, outputs}]` (topological order), `outputs: [{id, kind (inspector \| stage \| messages), stream_id}]` (a `messages` output is listed only while it offers a stream, see below), `status` (the latest status tick: flat `<node>.<metric>` keys, ADR-0011 §1.3), `stats: {samples, chunks, frames, gaps, discontinuities, skipped_samples, edits, status_ticks, decodes, decodes_dropped}` (`decodes`: Decode rows the `messages` outputs stored; `decodes_dropped`: frames dropped because a writer queue was full), `warnings`, `follow_hops`.

**`follow_hops`** (T-093/T-107, ADR-0011 §2.5, ADR-0013 §4.9 gap 11). `null` for a single-channel pipeline. For a follow-hops pipeline (`input.channels.mode = "follow-hops"`, one `follow_hops` node): `{channels: [{index, center_hz, bandwidth_hz}], channel_source, channel_bandwidth_hz, max_channels}`. `channels` is the running channel set, in the same shape `PUT …/channels` answers with. `channel_source` is how the set was last resolved: `"list"` (the recipe's `input.channels.list_hz`), `"hop-set"` (the target emitter's measured hop-set fingerprint) or `"detections"` (blind inventory detections ranked by sighting count within `input.channels.band_hz`, or the pipeline's tuned window if unset). `channel_bandwidth_hz` is `input.channels.channel_bandwidth_hz` (each channel's DDC bandwidth); `max_channels` is `input.channels.max_channels`, the chain-budget ceiling `PUT`/`refresh …/channels` enforce (`422 too_many_channels` beyond it). `PUT /api/pipelines/{id}/channels` and `POST /api/pipelines/{id}/channels/refresh` (above) change this set; they answer with `{channels, added, removed, applied_at_sample}` directly rather than nesting under `follow_hops`.

**Messages outputs** (ADR-0011 §2.2, T-111). A `messages` output stores Decode rows in the repository through the plugin decode ingestion (`hk_plugins::Ingest`), so recipe decodes reach `/api/inventory`, emitter identities and explanations the way plugin decodes do:
- **Rows.** One row per CRC-valid frame whose field map fit (`ok`/`partial`) and that carries every `decode.require`d field (by default, any mapped field). `decoder_id` is `recipe:<recipe id>` and `decoder_version` the recipe version. `frame_model` comes from the mapping, `identity` from `decode.identity` (canonical form of its scheme; an unknown token scheme becomes `other:<scheme>`), `metadata`/`content` from the mapped field paths (key = last path segment, or the whole path when two share it). No deduplication, as for plugins.
- **Gating.** The row carries the pipeline's `content_class` and is sanitised by the recipe's `output_policy` (the plugin manifest §9.3 rules) before the repository content gate.
- **Emitters.** The identity sighting uses the pipeline's target emitter as context. New emitters get the family step with `decode.service` (or the recipe id) as decoder evidence, like a plugin's manifest id.
- **Real time.** The pipeline thread only queues frames for the output's writer thread and never blocks; a full queue drops the frame and counts `decodes_dropped`.

**Hot edit** (ADR-0011 §2.3). The draft is a whole recipe document. The server plans it against the running revision (`plan.nodes[]`: `{id, change: unchanged | params-hot | params-cold | rebuilt | added | removed, keys?}`, `plan.reset`, `plan.field_maps_changed`, `plan.input_changed`, `plan.outputs_changed`). It builds new instances off the pipeline thread, which swaps graphs at its next chunk boundary.
- Unchanged nodes keep their state. Hot parameters, field-map content included, apply in place. Nodes downstream of a rebuilt node are reset.
- The ring reader never moves, so no sample is lost and capture never pauses.
- An `input` edit re-plumbs the channel DDC and rebuilds every node.
- Unchanged outputs keep their streams and consumers. Changed outputs get new streams, and removed ones finish.
- An `edit` record marks the boundary on every inspector stream, and later frames carry the new `edit_rev`. Edits don't save: use `POST /api/pipelines/{id}/save`.

**On-demand streams are the live edge, deliberately (T-387).** `/ws/open/<name>` openers (`listen`, `inspector?pipeline=`, `stage`) have **no history-window form**, and adding one would be an [ADR-0004](adr/0004-stream-output-contract.md) stream-contract change. T-387 asked whether any UI surface needs one and found none does:

- The **packet inspector** is the one surface whose records are *data about the air* — frame records carry capture-clock `t_ns` and are recorded to a capture — and its past-window form already exists as `GET /api/captures/{id}/frames?from_t&to_t` (above). A route that exists beats a contract change.
- `status` records are *telemetry of the decoder* (a node's lock/quality/error rate as it is reading now). They are stored verbatim in the capture file (stream contract §14.7) but **nothing indexes or serves them by time**, and a lock from an hour ago is not a stage's current state. The **pipelines list**, the **stage-status strip** and the **outputs dock** describe the run rather than the air, so they are honestly live-only — and the UI now *says so* on each of them rather than looking windowed. A surface that looks windowed while being live-only is the same class of error as claiming "no longer in the inventory" for a row that is merely outside the window (T-385).
- `GET /ws/open/inspector?capture=<id>&from_frame=<n>` is a **capture replay**, not a window: it is keyed by capture and frame, paces to the end of the recording, and is capped at 4 concurrent. A scrub wants the paged HTTP route.

Should a future task genuinely need decoder status by time, that is a **store + route** change (an index over `status` records), not a UI one.

**Streams of a pipeline** (stream contract §14; discovery lists them under `/api/streams`):

| Stream | How to open | Carries |
|---|---|---|
| `inspector/<pipeline>/<output>` | `GET /ws/inspector/<pipeline>/<output>` (the `/ws/{stream_id}` route), TCP `inspector/<pipeline>/<output>?token=…`, or the opener `GET /ws/open/inspector?pipeline=<id>[&output=<id>]` / TCP `open/inspector?…` (first inspector output by default) | messages stream, `message_schema: hackriff.inspector/1`: one frame record per frame, one `status` record per ~250 ms tick (every node batched), one `edit` record per applied edit |
| `stage/<pipeline>/<output>` | `/ws/stage/<pipeline>/<output>` or TCP (a recipe's declared `stage` outputs) | §14.4 binary records, one per processed chunk |
| `decodes/<pipeline>/<output>` | `/ws/decodes/<pipeline>/<output>` or TCP (a recipe's `messages` outputs, T-111) | messages stream, `message_schema: hackriff.decode/1` (as a plugin's `decodes/<plugin>`): one record per stored Decode row, republished through the stream gate. Under a class that forbids content it is offered only when the recipe's `output_policy` declares an allowlist; the rows are stored either way |
| on-demand stage tap | `GET /ws/open/stage?pipeline=<id>&node=<node>[&port=<port>][&view=raw\|spectrum\|sync_search\|eye][&sync_word=0x…&sync_bits=<n>][&symbol_rate_bd=<f>]` / TCP `open/stage?…` | `view=raw` (default), any node port: `iq` → `iq`/`cf32_le`, `real` → `audio`/`rf32_le`, `soft` → `symbols`/`rf32_le`, `bits` → `bits`/`ru8`, `frames` → frame records. `view=spectrum` (`iq`/`real` ports only, T-160): `kind: spectrum`, `rf32_le` dBFS/Hz rows, header `fft_size: 4096`, `sample_rate_hz` (declared row rate) ≤ 25, `bandwidth_hz` = the port's rate; `center_hz` is the channel's RF centre for an `iq` port and `0.0` for a `real` port (a demodulated baseband waveform — e.g. an FM MPX tap for the pilot/stereo/RDS subcarriers — has no RF reference). Computed server-side (hk-pipeline `recipes::tap_spectrum`) only while a consumer is attached, inline on the pipeline thread alongside the raw tap's own encode, so it never blocks or slows capture; 4096-point Hann-windowed, 50%-overlap segments are averaged (linear power) into one row at most every 1/25 s — resolving even a 57 kHz RDS subcarrier with wide margin at typical MPX rates. `view=sync_search` (`bits` ports only, T-162; needs `sync_word=0x…` and `sync_bits=<1..=64>`): `kind: sync-search`, `rf32_le` rows of the match score (`1 - errors/sync_bits`, `1.0` = perfect match) at every candidate bit position against the given word, header `fft_size` = row length (candidate positions per row, widened with the bit rate, then rows dropped past a memory cap, to hold `sample_rate_hz` — the declared row rate — to ≤ 25, the same cap as `view=spectrum`); no `center_hz`/`bandwidth_hz` (a bit-domain row). Computed server-side (hk-pipeline `recipes::tap_sync_search`) only while a consumer is attached, inline on the pipeline thread, at O(1) per bit (a shift, an XOR-and-mask, a popcount — the same work the `sync_search` block itself already does while searching). **Content, not metadata, unlike `view=spectrum`:** the caller picks `sync_word`, so an ungated score would let it probe withheld bits by trying candidates; it is gated exactly like the `bits` port it reads. `view=eye` (`iq`/`real` ports only, T-161; needs `symbol_rate_bd=<f>`): the **clock-recovery eye/timing diagram** — `kind: eye`, `rf32_le` rows, header `fft_size: 4096` = the row length, `sample_rate_hz` (declared row rate) ≤ 25, no `center_hz`/`bandwidth_hz` (the row's axes are time-within-a-symbol and amplitude, not frequency). A row is **64 consecutive traces of 64 points**, laid out trace-major; trace `k` is the waveform around the `k`-th symbol instant, resampled onto a grid spanning **2 symbol periods centred on that instant**, so point **32** is the symbol instant (where the eye is **open**) and points **16**/**48** are half a symbol either side (where it is **closed**). The instants are estimated server-side per row (the classical square-law/Oerder–Meyr non-data-aided estimate, no lock or training needed) and **reported**: the record's `sample_index` is the port element index of the row's first symbol instant, with the rest one symbol period apart — so the eye can be lined up against the same port's `view=raw` tap. The estimate is sub-sample and the traces are folded on it at full precision; only the reported `sample_index` is rounded to the nearest port element, because it is an integer index. Computed server-side (hk-pipeline `recipes::tap_eye`) only while a consumer is attached, inline on the pipeline thread, at one multiply-accumulate per sample (a stepped rotator, no transcendentals) plus one fold per row; rows are dropped whole once the symbol rate would exceed the cap, so a row always shows 64 consecutive symbols but successive rows need not be contiguous. **Content, not metadata, like `view=sync_search` and for a blunter reason:** point 32 of every trace *is* the pre-decision soft symbol in symbol order, so slicing that one column of a row recovers the demodulated bitstream outright; it is gated exactly like the `iq`/`real` port it is folded from. Every tap costs nothing until opened and stops when the consumer leaves. 404 unknown pipeline/node/port, 410 ended, 400 an unrecognised `view`, a missing/invalid `sync_word`/`sync_bits`, or a missing/invalid `symbol_rate_bd` (it must give 2..=1024 samples per symbol on that port: below 2 there is nothing between the instants to draw, above 1024 the window a row buffers is unreasonable — decimate first), 409 a `view` the port type doesn't support (`spectrum` and `eye` on `soft`/`bits`/`frames`; `sync_search` on anything but `bits`) |

### Recipe matching (T-164, ADR-0011 §2.4, ADR-0013 §4.9 gap 7b)

`GET /api/recipes/match?emitter=<id>` ranks every recipe in the store against **what has been measured on one emitter**, so the workbench can suggest a decoder without the user knowing which one to reach for. Code: `hk_recipe::matching` (all the arithmetic, unit-tested on its own), `crates/hk-api/src/recipes.rs` (gathering the measurements and shaping the answer).

- **A suggestion, never an action.** Nothing here tunes, retunes or starts a pipeline; the user (or an explicit scan-plan rule) does that with `POST /api/pipelines`. A ranking sets no identity, family or status.
- **Measurement decides the order.** The score is built only from measurements: the classified modulation family, the bandwidth (the demodulator's channel filter when a session has run, otherwise the detected extent), the estimated symbol rate, burstiness derived from a **measured** duty cycle, and feature tokens the estimator evidenced (`pilot-19k` from a locked 19 kHz pilot). A recipe's `freq_hz` is a band-plan prior about where such signals usually live, **carries no weight at all**, and survives only as `band_hint`: it breaks a tie between candidates the measurements cannot separate and does nothing else. This is the T-212 rule for C17 classification priors, applied to recipes — a prior may never promote a worse-matching recipe over a better-matching one.
- **Unmeasured is not agreement.** Each expectation a recipe *declares* opens one weighted slot; a slot whose measurement exists earns `weight × exp(−z²/2)` (where `z` is the normalised distance from the declared range, `0` inside it), and a slot whose measurement is missing earns nothing while its weight still counts in the denominator. `score = Σ earned / Σ declared weight`, so it is absolute and comparable between recipes rather than "best of what was found", and a recipe declaring five expectations of a signal with one measured parameter scores low by construction. T-163 serves `null` for what was never measured precisely so this holds; nothing substitutes a default. `z ≤ 1` reads as agreement and `z > 3` as an active conflict, the same normalised-distance vocabulary as `/api/signatures/match`.
- **Nothing fits ⇒ nothing is offered.** `recipes` is **empty** when no candidate clears the floor: a weak fit is reported as nothing to offer, never as the best of a bad set. A recipe is not offered when the measured modulation family is one it cannot decode (`family_conflict`), when fewer than two of its expectations have been measured (`too_few_compared` — one agreeing field out of five declared is a coincidence), when it scores below `0.2` (`low_score`), or when it declares no expectations at all (`no_match_hints`). `outcome: "none"` means there is nothing to suggest, **not** that the emission is unknown.
- **Modulation labels are folded carefully.** A recipe declaring a whole family (`fsk`) accepts any class in it (`2fsk`, and the legacy spelling `fsk2`); a recipe declaring a class is satisfied only by that class, because `am` and `wfm` are sibling classes of `analog` and widening them would let an ACARS recipe claim an FM station. A label that does not resolve the expected modulation — a coarser family, or a **service** label such as `fm-broadcast` which is not a modulation label at all — counts as no evidence either way, never as a conflict.
- **Gating.** An emitter whose identity is withheld contributes no demodulation session, exactly as its `estimated_params` read `null` on `/api/inventory/{id}` (T-036/T-163). Its detection-level measurements are still used, since the same row already serves those in clear, so this route adds no way to tell a withheld emitter from one nothing has demodulated yet.

**Response**: `{emitter, measured, outcome, reasons, recipes, ruled_out}`.
- **`measured`** is the input the ranking read, echoed back: `{family, family_source ("classification" | "demodulation" | null), f_center_hz, bandwidth_hz, bandwidth_source ("demodulation" | "detection"), symbol_rate_bd, bursty, duty_cycle, features: [...], session}`. Anything unmeasured is `null`.
- **`outcome`** is the best candidate's, or `none` when there are none.
- **`reasons`** are machine codes for the ranking as a whole: `no_candidate`, `too_few_measurements`, `ambiguous_candidates`, `all_partial`.
- **`recipes`** (best first, possibly empty): `{id, version, name, score, outcome ("fit" | "partial" | "none"), compared, agreed, conflicting, unmeasured, band_hint, reasons: [...]}`. A `fit` needs a score ≥ 0.8, no conflicts and at least two compared fields; anything else ranked is `partial`.
- Each candidate **reason** is `{field, verdict ("agree" | "near" | "conflict" | "unmeasured"), measured, expected, z, weight, earned, detail}`, where `field` is `family`, `bandwidth_hz`, `symbol_rate_bd`, `bursty` or `feature:<token>`, and `detail` is rendered from those numbers and names no identity.
- **`ruled_out`**: `{id, version, name, reason, detail, score}` — so the workbench can say *why* a recipe is not offered.
- **Errors** `{"error", "code"}`: `400 invalid` (no `emitter`), `404 not_found` (unknown or unparsable id), `503 unavailable` (this server has no inventory, or no recipe runtime), `405` other methods. Messages never echo values.

## Decoder workbench (planned, M1; ADR-0011)

**Planned, not served yet.** None of these routes are in `ROUTES` today (the served T-088 and T-089 routes moved to "Recipes and pipelines" and "Inspector" above). They are named here so the parallel M1 tasks and the inspector UI (T-090) code against one surface. When an owning task lands, it moves its rows into a normal section with request/response shapes and contract tests.
- **Contracts:** [ADR-0011](adr/0011-decoder-workbench-contracts.md).
- **Schemas:** `hk_recipe` (recipe, field map, block descriptor types).
- **Wire formats:** [`docs/stream-contract.md` §14](stream-contract.md) (inspector frame records, stage streams, recorded decoded streams).

Conventions:
- **Bodies and errors.** Recipe and field-map bodies are the JSON documents themselves, within the 64 KiB body cap. A validation failure is `400 invalid` with `errors: [{path, message}]` and `warnings`. Messages never echo values.
- **Times.** Unix seconds (floats), as elsewhere in this document.
- **Audit.** Mutating routes are audited like every other.

| Method | Path | Owner | Purpose |
|---|---|---|---|
| POST | `/api/assist/sync` | T-091 | Sync-word and period suggestions over a capture's frames or bits, scored |
| POST | `/api/assist/fields` | T-091 | Entropy-based field-boundary suggestions as field-map fragments, scored |
| POST | `/api/assist/crc` | T-091 | CRC/BCH parameter search over a capture's frames → `crc`/`bch` parameter objects, scored |

Assist suggestions are never applied automatically: the user accepts or edits them into a recipe, which then goes through `POST /api/recipes/validate`.

### Authoring assist (T-091)

Classical, compute-only helpers (`hk_estimate::assist`) for writing a parser over recorded bits. They answer **scored suggestions** with reasons, never truth, and save nothing, so they need the token in the `Authorization` header like every POST but are not audited. Nothing is looked up by protocol: the search measures structure blind, and catalogues only *name* what it measured (`known_as`, `reveng.name`, `cyclic.name`).

**Input.** `Content-Type: application/json`; unknown keys are `400 invalid`. Bits are air order.
- `bits`: one stream, `"0101…"` or `{"hex": "…", "bit_len": n}` (hex digits MSB first; `bit_len` trims the tail);
- `frames`: an array of `{"bits": "0101…"}`, `"0101…"` or `{"hex", "bit_len"}` in capture order (the frame packing rule: MSB of byte 0 first). Capture ids (T-092) are not accepted yet: post the frames.
- Limits: ≤ 400 000 bits and ≤ 20 000 frames per call (the 64 KiB body limit usually binds first).
- `max_ops` (optional): the work cap in operations, charged at roughly 1 ns of release-build time each in the most expensive stages (default 5 × 10⁸, about ≤ 1 s; ceiling 1.5 × 10⁹, about ≤ 2–3 s; larger values are clamped). Every answer has `work {ops, max_ops, partial, hypotheses, skipped[]}`; `partial: true` means the cap was hit and the answer holds what was found before it; `skipped[]` also says what was not analysed because of the input (e.g. frames longer than 4096 bits for `fields`).
- **Concurrency:** the search runs on the HTTP connection thread, so **one assist call computes at a time**. A call that arrives meanwhile answers at once with `503 {"error", "code": "busy"}` (no queueing); retry after the running call finishes.

**Scores are absolute and honest** (0–1, comparable across answers): noise, repeated frames and too few frames score near 0, not "best of what was found". Ranking within an answer may use a relative key (`relative_score` for syncs).

**Fragments.** Suggestions carry `fragment: {block, params}` in the pinned block-parameter shapes (ADR-0011 §1.5): `sync_search` (`sync-word` or `offset-words`), `crc` (RevEng model with `span` or `blocks`), `bch` (`word_bits, n, k, poly, parity`) and `parity`. Hex values are `0x…` strings.

| Route | Body | Answer |
|---|---|---|
| POST `/api/assist/sync` with `bits` | `{bits, max_errors?, max_sync_bits? (≤ 64), max_block_bits? (≤ 128), max_lag?, max_ops?}` | `{input: "bits", bit_len, syncs[], periods[], block_period?, block_codes[], block_parity[], offset_words?, work}` |
| POST `/api/assist/sync` with `frames` | `{frames, max_errors?, max_sync_bits?, max_ops?}` | `{input: "frames", frames, syncs[], work}` |
| POST `/api/assist/fields` | `{frames, align?: {sync: "0101…", max_errors?}, find_crc? (default true), max_ops?}` | `{frames, frames_given, frames_aligned, byte_structured, fixed_length, per_bit[], suggestions[], field_map, field_map_errors[], codes[], work}` |
| POST `/api/assist/crc` | `{frames, min_width? (3), max_width? (32), max_tail_bits? (16), max_classes? (8), max_ops?}` | `{frames, codes[], parity[], work}` |

- **`syncs[]`** (best first): `{bits, bit_len, hex, hex_lsb_first?, complement_hex, kind (sync | repeat | fill), occurrences, inverted_occurrences, max_errors, modal_interval_bits?, regularity, preamble_fraction, preamble_bits, frames_with?, modal_offset?, evidence_bits, significance_bits, score, relative_score, reasons[], fragment}`. Seeds are counted with the complement folded in (an inverting demodulator finds the same word); `hex_lsb_first` reads the bits as LSB-first bytes; `fill` marks words that repeat back to back (idle codewords). Both counts are chance-corrected: the expected occurrences of a pattern of that width (with `max_errors`, either polarity) in random bits of the same lengths (frames mode: frames holding it) are subtracted. `evidence_bits` = excess occurrences × information per occurrence, the ranking key. `significance_bits` = −log2 of the Chernoff bound on that many occurrences by chance (Poisson over a stream, binomial over frames) minus the pattern width (look-elsewhere). **`score`** = `1 − e^(−significance_bits/32)`, absolute (random bits ≈ 0); **`relative_score`** = the ranking key relative to the best sync of this answer.
- **`periods[]`**: `{period_bits, offset_bits?, method (linear-block | autocorrelation), agreement?, z?, rank?, deficiency?, constant_columns?, rows?, harmonic_of?, evidence, score, reasons[]}`; `score` is relative to the strongest period of the same method (the absolute thresholds are the gates: ≥ 8 σ autocorrelation, ≥ 12 evidence bits linear-block). `linear-block` means stacked `period_bits`-bit blocks at `offset_bits` have deficient GF(2) rank: they are codewords of a linear code (RDS 26-bit blocks, POCSAG 32-bit codewords). The stream answer then runs the code search over those blocks (`block_codes`, `block_parity`) and, when the best code has per-position constants, proposes an `offset_words` `sync_search` fragment.
- **`codes[]`**: `{kind (crc | bch), width, generator (with the x^w term), poly (without), start_bit, tail_bits, bit_order (air | byte-reflected), classes, init, xorout, init_resolved, class_constants[], cross_length, method (exhaustive | catalogue), cyclic? {n, k, covered_bits, name?}, known_as?, reveng? {name, params, byte_order, field_endianness}, validated, tested, differences, evidence_bits, score, ambiguous_with?[], reasons[], fragment}`. Frames are counted once however often (and in whichever class) they repeat; `differences` = validated distinct frames − constants, not counting differences that repeat with a period ≤ 16 bits (constant/alternating input), and `evidence_bits = width × differences − log2(hypotheses)` (≥ 16 to be listed). **`score`** (absolute) = validated share × `(1 − e^(−evidence_bits/16))` × a posterior share × a chance-factor confidence. Every divisor of a fitting generator fits too, and with few differences their GCD carries chance factors (3 Mode-S frames: CRC-24 × a small factor fits), so the generators fitting one hypothesis (same `start_bit`, `tail_bits`, `bit_order`, `classes`) compete. Each gets its **posterior share** with weight `2^((k−1)·width)`, where `k` is the fewest `differences` among them: a multiple beats its divisor by `2^(d·(k−1))` for a degree gap `d`, because every difference must carry the extra factor. A generator with a repeated irreducible factor pays `2^−10`: designed generators are squarefree, and a chance factor already present in the true generator squares it (8 Mode-S frames: (x+1)·CRC-24 fits in 1 of 128 draws). The **chance-factor confidence** `exp(−6·Σ_j I_j·2^(−j·k))` (`I_j` irreducible polynomials of degree `j`) is ≈ 0.1 at 3 frames, 0.4 at 4 and 0.95 at 8. Generators holding ≥ 0.05 of the posterior are listed. **`ambiguous_with`** (omitted when empty) lists the full-form generators of the other listed generators of the same hypothesis: an explicit ambiguous group, each with a reason naming the best one and its share. Suggestions are ordered by `score`, then `evidence_bits`, then `generator`. Values are bit-serial over the air-order bits; `reveng` maps a byte-aligned CRC onto the RevEng model when one reproduces the frames. `classes > 1` means frames grouped by index mod `classes` each have their own constant (`class_constants`, e.g. RDS offset words).
- **`parity[]`**: `{scope: frame {start_bit, tail_bits} | character {char_bits, phase}, parity, validated, tested, score, fragment}`.
- **`suggestions[]`** (fields): `{name, kind (constant | counter | length | high-entropy | mixed | check), bit_offset, bit_len (null = varies), from_end, value_hex?, length_scale?, length_add?, mean_entropy, mean_constancy, score, reasons[]}`; `field_map` is the matching draft `FieldMap` (unit bits) and `field_map_errors` its static validation errors. `per_bit[]` is `{coverage, ones, entropy, transition}` per bit position for plotting. Structural scores are chance-corrected with `1 − e^(−bits/8)` of significance bits: `constant` × P(random bits this constant over this many frames, any start), `counter` × P(random values stepping by one on that many pairs), `length` × P(random values matching the lengths), `check` = the code score; `high-entropy` is the mean entropy and `mixed` 0.5 (descriptive, not claims). With `align`, frames are aligned inside the work cap (`frames_aligned` counts the frames kept). Frames longer than 4096 bits are not classified (empty answer, reason in `work.skipped`).
- **Errors:** `400 invalid` (bad bits, unknown key, both or neither of `bits`/`frames`), `415` (not JSON), `405` for other methods, `503 busy` while another assist call is computing.

## Decoded captures (T-092, M1; ADR-0011 §4, stream contract §14.7)

**Every running pipeline's inspector output is recorded automatically** ("always recorded", docs/13 layer 4), so a parser can be authored and re-run over what was actually decoded. Recording, index and quota live in `hk_store::decoded`; `hk_pipeline::recipes::capture` tees each inspector stream in when a pipeline starts or a hot edit adds an output; these routes only route, audit and shape errors (`crates/hk-api/src/captures.rs`).

- **Storage** (`<data dir>/captures/`). `<id>.hks` is the §3 byte stream itself: the header frame, then the records as published. Frame records are stored without `content.layers` (derived; re-parse recomputes them). `<id>.idx` is the frame index: one 16-byte little-endian entry per frame record, `u64` byte offset + `i64` `t` (Unix ns), so frame and time scrubs seek. `<id>.json` is the catalogue entry.
- **Never blocks capture.** The recorder is a local consumer of the stream's publisher, with a bounded queue (4 MiB) and its own writer thread. A slow disk makes the publisher drop records; drops are counted in `dropped_records`, and the pipeline never waits. The publisher's slow-consumer disconnect does not apply to the recorder, so a disk stall of any length loses records, never the recording.
- **The books balance whatever ends the capture (T-465).** `frames + dropped_records` is what the pipeline published on that stream. Closing the recorder frees whatever was still queued — the publisher's 5 s drain timeout at stream end (`end_reason: "drain-timeout"`), or a detach — and those records, plus any drop the recorder had not yet been told about, are counted into `dropped_records` at that point rather than vanishing. A capture that ends `finished` lost nothing and reads `dropped_records: 0` unless the disk could not keep up. The drain timeout is not lengthened to hide this: no record is queued after the stream finishes, so the drain is already bounded by one queue, and the timeout only bounds a *write that may never return*.
- **Failures.** A failed write never stores a byte twice; the capture ends (`write-failed`) at its last complete record.
- **Content rule.** The §6 gate runs before storage. A frame whose class forbids content is stored metadata-only and can never be re-parsed.
- **Quota.** Defaults: 1 GiB total (`$HK_DECODED_CAPTURE_TOTAL_BYTES`) and 64 MiB per capture (`$HK_DECODED_CAPTURE_BYTES`, clamped to at most a quarter of the total).
  - At the per-capture size a recording **rolls** to a new capture: a new id, `segment + 1` and the same header.
  - Past the total, the **oldest finished captures are evicted first**. If only recording captures remain, their segments roll and new records are dropped and counted until the store is back under quota.
  - A capture that ends with no frame records is deleted.
  - When `hk serve` starts, captures a previous process left recording are closed with `end_reason: "interrupted"`. Each is first cut back to its last complete, indexed record: a torn tail record or partial index entry left by a power loss is removed, and `frames`, `bytes` and `t_first`/`t_last` come from what is kept.

| Method | Path | Auth | Answers |
|---|---|---|---|
| GET | `/api/captures` | token | `{"captures": [Capture]}`, newest first |
| GET | `/api/captures/{id}` | token | `Capture`; 404 `not_found` |
| DELETE | `/api/captures/{id}` | token (audited `capture_delete`) | `{"deleted": Capture}`; 409 `conflict` while it is still recording; 404 `not_found` |
| GET | `/api/captures/{id}/frames` | token | `?[from_frame=<n> \| from_t=<unix s>][&to_t=<unix s>][&limit=1..500, default 100]` → `{capture_id, capture, stream, total_frames, from_frame, limit, next_from_frame, frames}` |

**`Capture`** fields:
- `id` (`[A-Za-z0-9_.:-]{1,128}`), `pipeline_id`, `recipe_id`, `recipe_version` (at the capture's start; frames carry their own `metadata.recipe_version`/`edit_rev`), `output_id`, `stream_id`, `content_class`, `segment`.
- `started`, `ended` (`null` while recording), `t_first`, `t_last` (the first and last frame's time), all Unix **seconds** — so this object is seconds while the `frames` it describes carry `t_ns` in nanoseconds; the names are what distinguish them (T-354).
- `frames` (frame records stored), `bytes` (stream bytes stored), `dropped_records`, `recording`.
- `end_reason`: `finished`, `rolled`, `slow-consumer`, `write-failed`, `drain-timeout`, `detached` or `interrupted`.

**Scrubbing** (`/frames`):
- `from_t` (and `to_t`) are Unix **seconds**, like every bare time name in this document; a frame record's `t_ns` is nanoseconds, so scrub with `t_ns / 1e9`. `from_t` resolves through the index to the first frame at or after that time. The response's `from_frame` says which frame that is, so a time scrub is also a frame position. A nanosecond value passed here is refused (`400 invalid`) rather than silently misread: the parameter is bounded to |t| < 9×10⁹ s.
- `to_t` ends the page at the first frame after it (`next_from_frame: null`). Give `from_frame` or `from_t`, not both.
- `frames` are the stored frame records in order, served fail closed like the parse route: a record whose class (or the stream's) forbids content, or that is `own-key-decrypted`, is `gated: true` without `content`.
- `stream` is as in the parse route, with `inspector.source: {kind: "capture", capture_id, reparse: false}`.
- **This is the packet inspector's window (T-387).** The UI's packet inspector is a view over the one (time × frequency) window like every other surface, and it reaches a past window through **this** route: `GET /api/captures` gives the `pipeline_id` → capture key, then `from_t`/`to_t` (both ends closed, on the capture clock) give the window's frames. It merges them with what its live socket received, de-duplicated, so a window straddling the live edge lists each frame once. Nothing re-decodes: these are records the pipeline already wrote, which is what the incremental-decode invariant asks for. **No stream-contract change was needed**, and none was made — see "on-demand streams are the live edge" under `/ws/open/<name>` below.

**Re-parse** with a draft field map is `POST /api/captures/{id}/parse` ("Inspector" above). It uses the index too: without a field map only the requested page is read; with one, the fit pass reads the first 100 000 frames and a page past them is a second seek.

**Replay stream**: `GET /ws/open/inspector?capture=<id>[&from_frame=<n>][&field_map=<recipe_id>@<version>:<map_id>]` (TCP `open/inspector?capture=…&token=…`).
- It replays the stored frame records from frame `n` (an index seek). With `field_map` (a saved recipe version's map), each record is re-parsed: `content.layers` and `metadata.fit` are added.
- The header is the recording's, with `stream_id: capture/<id>` and `inspector.source: {kind: "capture", capture_id, reparse}`. The §6 gate runs again.
- `seq` is the replay stream's own (from 0, for its drop detection). `t`, `metadata` (`frame`, `sample_index`, `recipe_version`, `edit_rev`, ...) and `content` are the recording's.
- Records are paced to the consumer rather than dropped. The stream finishes after the last frame stored when it opened. Only frame records are replayed (not `status`/`edit`).
- Refusals: 400 `bad-request` (bad `capture`, `from_frame` or `field_map` syntax), 404 `not-found` (capture, recipe version or map), 422 `unreadable`/`invalid`, 503 `busy` (4 capture replays are already streaming on this server; no queueing, retry when one ends, like assist's `busy`).

Errors are `{"error", "code"}`: `400 invalid` (bad id or query, messages never echo values), `404 not_found`, `405` (with `Allow`), `409 conflict`, `422 unreadable` (not a readable inspector stream), `500 unreadable` (store I/O), `503 unavailable` (no capture store on this server).

## Observation log (T-115; ADR-0012 §1)

Where and when the radio actually observed, and why: one `DwellRecord` per non-sweep scheduler step and one `SweepRecord` per discovery pass or 60 s, whichever ends first (hops reference a `SweepGeometry`). Schemas: `hk_model::attention::observation`. Records come from the scheduler of a scheduler-driven run (`hackriffd`, `hk run --schedule`); a live run without the scheduler (`hk serve`) logs its interactive tuning as `interactive`-tier dwell records (one per steady tune, closed on retune and split every 60 s; they add coverage and observed seconds, never activity-independent visits); a recording replayed without the scheduler logs nothing. Frequencies in Hz; times in Unix seconds on the sample clock the captured blocks carry (a replay reports the recording's time).

- **Observed extent.** `window.usable` is the analysed spectrum frame's extent (the span history tiles fold, so log and tile coverage describe the same cells), clipped to the sampled band; `window.dc_excluded` is the ±15 kHz DC notch. A frequency range counts as observed only while it lies **entirely** inside `usable` minus the notch.
- **Observed interval.** `observed` starts when the analysed data carries the step's tuning (retune settle) and ends when the next step starts, clipped to `planned`; `preempted` marks a step cut before its planned end. Sweep `visits[]` give `start_ms`/`observed_ms` after the record's `span.t0`.
- **Freshness.** Queries read the segment files plus the writer's unflushed buffer. Records are dropped (and counted in `log.dropped`) only when the writer queue is full; the pipeline never waits for the log.
- **Storage.** Hourly CRC-line segments `<data>/observations/YYYY/MM/DD/HH.log`, flushed at most once a minute (or at 256 KiB), fsynced at hour seal, kept **180 days / 2 GiB** by sample time (T-406, `docs/16` §5.4 — raised from 30 days / 512 MiB so the coverage record outlives the spectrum-history pyramid it explains; lower them with `ScanPlan.extra.pipeline.observation_retention_days` / `observation_max_mb`). **Which bound binds depends on the policy, and it is measured, not assumed:** an iterative scan (T-406) writes one 618-byte line per step, so at a 10 s dwell that is ~5.3 MB/day and the quota holds ~400 days — the age binds first. A continuous 50 ms-hop sweep aggregates up to ~1500 hop visits into one record and is ~2 orders of magnitude denser per day, so there the **quota** binds and raising the age alone would not have moved that horizon.

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/api/observations?f_lo&f_hi&t0&t1[&tier][&cursor][&limit]` | token | Records overlapping the box, in log order |
| GET | `/api/observations/coverage?f_lo&f_hi&t0&t1[&channel_hz][&tau_s][&min_gap_s]` | token | `ObservationTotals`, per-channel totals, gaps and POI rows |

`GET /api/observations` → `200`:

```json
{
  "f_lo_hz": 100000000.0, "f_hi_hz": 102000000.0, "t0": 1789300800.0, "t1": 1789300860.0,
  "records": [
    { "record": "sweep", "schema": 1, "survey_id": "…", "plan_version": 1, "site": { "kind": "unassigned" },
      "geometry": 1234567890123, "span": { "start_ns": 1789300800000000000, "end_ns": 1789300860000000000 },
      "visits": [ { "hop": 0, "start_ms": 0, "observed_ms": 50 } ],
      "preempted_hops": 0, "dropped_samples": 0, "overload_hops": 0 },
    { "record": "dwell", "schema": 1, "seq": 42, "plan_version": 1, "site": { "kind": "unassigned" },
      "reason": { "code": "poi-dwell", "poi": 3 }, "tier": "bandit",
      "window": { "center_hz": 100500000.0, "sample_rate_hz": 2400000.0,
                  "usable": { "lo_hz": 99300000.0, "hi_hz": 101698828.1 },
                  "dc_excluded": { "lo_hz": 100485000.0, "hi_hz": 100515000.0 }, "rbw_hz": 3515.6 },
      "rf_path": 0, "planned": { "start_ns": 1789300800000000000, "end_ns": 1789300830000000000 },
      "observed": { "start_ns": 1789300800100000000, "end_ns": 1789300830000000000 },
      "preempted": false, "dropped_samples": 0, "overload": false }
  ],
  "geometries": [ { "schema": 1, "id": 1234567890123, "plan_version": 1, "hops": [ { "center_hz": …, "sample_rate_hz": …, "usable": {…}, "dc_excluded": {…}, "rbw_hz": … } ] } ],
  "next_cursor": null,
  "truncated": false,
  "log": { "offered": 120, "dropped": 0, "written": 120, "flushes": 2, "sealed": 0, "write_errors": 0, "segments_deleted": 0, "bytes": 48213 }
}
```

- `tier`: `interactive`, `pinned-lease`, `scheduled-plan`, `bandit` or `background-sweep` (sweep records are `background-sweep`). A sweep record matches when any visited hop's `usable` overlaps the box; `geometries` holds every geometry the page's sweep records reference.
- `limit` defaults to 1000, at most 10000; `next_cursor` (a record offset) is set when more records match.
- Times inside records (`span`, `planned`, `observed`, and `totals.span`) are hk-model `TimeRange`s, so each is `{start_ns, end_ns}` — integer Unix **nanoseconds**, named per the units convention. The top-level `t0`/`t1` and `gaps` are seconds.
- **`f_lo_hz`/`f_hi_hz`** (T-355; previously bare `f_lo`/`f_hi`, which the units convention above already required for the `_ns`/`_s` time family but had not been applied to frequency): the requested box, echoed in Hz. Matches `FreqRange`'s own field names (`lo_hz`/`hi_hz`), prefixed the way every other envelope-level frequency field on this API already is (`f_lo_hz`/`f_hi_hz` on inventory rows, tiles, `/api/analysis/strongest`, …), so a field can no longer be renamed or dropped without a contract test failing.

`GET /api/observations/coverage` → `200`:

```json
{
  "f_lo_hz": 100990000.0, "f_hi_hz": 101010000.0, "t0": 1789300800.0, "t1": 1789300860.0,
  "totals": { "freq": { "lo_hz": 100990000.0, "hi_hz": 101010000.0 }, "span": { "start_ns": 1789300800000000000, "end_ns": 1789300860000000000 },
              "n_visits": 58, "n_visits_activity_independent": 58,
              "observed_s": { "interactive": 0.0, "pinned_lease": 0.0, "scheduled_plan": 0.0, "bandit": 0.0, "background_sweep": 2.9 },
              "max_gap_s": 1.1, "mean_revisit_s": 1.03 },
  "channels": null,
  "gaps": [ { "t0": 1789300800.05, "t1": 1789300801.0 } ],
  "gaps_truncated": false,
  "poi": [ { "tau_s": 0.01, "p_poi": 0.058 } ]
}
```

- A **visit** is a maximal run of contiguous observed time covering the range: consecutive hops that both cover it are one visit. `n_visits_activity_independent` counts visits that include a `background-sweep` or `scheduled-plan` observation (usable for occupancy without revisit bias). `observed_s` sums observed seconds per tier inside the span; `max_gap_s` counts the span edges; `mean_revisit_s` is the mean start-to-start interval (absent with fewer than two visits).
- `channel_hz` tiles `f_lo..f_hi` into channels (at most 4096) and returns one `totals` object per channel in `channels`.
- `gaps`: unobserved intervals of at least `min_gap_s` (default: any), at most 1000 (`gaps_truncated`).
- `tau_s`: up to 16 comma-separated burst durations; each `poi` row is the fraction of burst start times in the span whose burst overlaps an observation (`hk_model::attention::schedule::poi_fraction`).
- `400 invalid` for a missing or bad `f_lo`/`f_hi`/`t0`/`t1` (`f_hi > f_lo ≥ 0`, `t1 > t0`), `tier`, `cursor`, `limit`, `channel_hz` or `tau_s`; `503 unavailable` when the server has no observation log; `405` for other methods.

### Stream `presence` — interval endpoints (T-388, rebuilt by T-410)

Stream id `presence` (`/ws/presence`, ADR-0004 `messages` kind, `message_schema` `hackriff.presence/3`, `content_class` `unrestricted`, listed by `GET /api/streams`; full spec: [stream contract §15](stream-contract.md), rationale: [ADR-0019](adr/0019-presence-as-an-interval-with-endpoints.md)). One record when an emitter's presence interval **opens**, and one when it **closes** — and **nothing at all while it continues**:

```json
{"type":"message","seq":7,"t_ns":1757774400123456789,"emitter_id":"0199…",
 "content_class":"unrestricted","gated":false,"frame_model":"presence-end",
 "metadata":{"kind":"presence-end",
             "last_interval":{"t_start_s":1757774390.1,"t_end_s":1757774400.12,"open":false,"revoked_s":0.0}}}
```

**Why it exists, and what changed.** `GET /api/inventory` is polled, and an open track reaches the inventory every 5 s (`LIVE_OFFER_NS`), so a live signal's box used to grow in steps of 5–10 s even though detection runs on every STFT frame. T-388 fixed that by pushing the measured top forward once per tick. T-410 replaced the model: **presence is an interval with endpoints**, so an open interval's box runs from its start **to the live edge** and caps only on a detected end — the measurement is the opening record plus the *absence* of a closing one. The poll still creates, arbitrates, merges, confirms and **windows** the rows; it may now also **cap** a box, and is the backstop for a lost close.

- `metadata.kind` and `frame_model` are `presence-start` (a first interval on this emitter), `presence-reopen` (a later one, after a silence longer than the revocation window), `presence-end`, or `presence-revoke` (T-413: the END published within the last idle gap is **withdrawn** — the signal came back inside the revocation window, so the interval it capped is the *same* interval and is open again). `open` is `false` only on `presence-end`; a record whose `open` disagrees with its `kind` is malformed.
- **An END is provisional for one idle gap.** A consumer must not file it as final before then: a `presence-revoke` may withdraw it, carrying the interval's **original** `t_start_s` — which is what distinguishes it from a `presence-reopen`, where one box grows rather than a second appearing. Its `revoked_s` is a measured **lower bound** (the silence the receiver watched before deciding); the next inventory poll states the whole gap.
- `metadata.last_interval` is the **same object** the row's `presence.last_interval` is, above — assign it, do not rebuild it. The envelope's `t_ns` is the instant the record is *about*: `t_start_s` for an opening record, `t_end_s` for a closing one, in integer Unix nanoseconds (the units convention: `_s` seconds, `_ns` nanoseconds).
- **A close names the end actually measured, never the instant it was decided and never a clock read.** So a box that has been running to the live edge **retracts** to the measured end when the close lands. A consumer must **not** read a clock or extrapolate past the live edge, and **must draw the span above `t_end_s` as assumption** (`ui/src/timebox.ts`'s open cap): while the interval is open that span is the one thing on screen nobody measured, and it grows as the silence grows.
- **How long a box may over-claim.** An interval closes after one idle gap of *observed* silence, measured off the run's tune history per band (`hk_model::IdleGap::from_coverage`): **1 s** where the receiver never looked away, `2 ×` the revisit period where it did, and only 60 s where no coverage was recorded at all. So a box over-claims **≤ 1.25 s** typically (gap + one tick) and **≤ 5 s** if the close is lost and the next poll is what caps it.
- **A reopen is not a new emitter.** Whether a return is a new *interval* is answered by the idle gap; whether it is the same *emitter* is answered by entity resolution. A reopen replaces `last_interval`, so the returning signal gets its **own box** and the silence between them is drawn as a gap — never one box stretched across it.
- **Time only, no frequency.** An endpoint is new time, not new geometry; the box's frequency edges come from the row.
- A track with no inventory row publishes nothing. A track that closes, merges or joins a hop set publishes its **close**, so there is no path that leaves a box open forever. A track that *returns* after its own close publishes nothing more — the only start it could offer is its first burst, on the far side of the silence just capped — so the next poll serves that new interval with its real start.
- **Rate:** at most one tick per 250 ms and at most 32 records per tick — 128 records/s, whatever the band is doing. Closes are published before opens, and what a tick cannot fit is **carried to the next**, not dropped: an unsent close is an over-claim, where an unsent open is only a box that appears a tick later. A slow consumer is dropped, never the survey.
- Only a **following** view should subscribe; a paused or scrubbed view is answering about a fixed past window, where every endpoint is already known, and stays on the poll.

Stream `observations` (ADR-0004 `messages` kind, `message_schema` `hackriff.observation/1`, metadata only, listed by `GET /api/streams`): one message per record the log writes, published from the writer thread (a slow subscriber drops messages, never log records). `metadata.kind` is `dwell` (`reason_text`, `record`: the `DwellRecord`) or `sweep-summary` (`plan_version`, `geometry`, `t0_s`, `t1_s`, `visits`, `observed_s`, `f_lo_hz`/`f_hi_hz` of the visited hops, `preempted_hops`, `dropped_samples`, `overload_hops`). Geometry records are not streamed; read them from `GET /api/observations`.

## Occupancy (T-118; ADR-0012 §2)

ITU-R SM.1880 / SM.2256 occupancy per learned channel and per band, computed in the backend (`hk_context::occupancy`) from the level-0 spectrum history, the run's detections and the observation log. Channels are learned blind from detections; band rasters appear only as `raster_hint` suggestions.

| Method | Path | Auth | Returns |
|---|---|---|---|
| GET | `/api/occupancy?f_lo&f_hi&t0&t1[&subject=channel\|band][&interval=15m\|1h\|span][&site]` | token | `OccupancyStat` rows overlapping the box |
| GET | `/api/channels?f_lo&f_hi[&site]` | token | The learned channel plan overlapping `f_lo..f_hi` |

`GET /api/occupancy` → `200`:

```json
{"interval": "15m", "f_cell_hz": 6250.0, "plan_version": 3, "truncated": false,
 "rows": [{"schema": 1, "site": {"kind": "unassigned"}, "subject": {"kind": "channel", "key": {"scheme": 1, "lo_cell": 69358, "hi_cell": 69362}},
           "interval": {"start_ns": 1789300800000000000, "end_ns": 1789301700000000000}, "fco": 0.12, "fco_all_visits": 0.31, "fco_suspect_upper": 0.13, "fbo": 0.08,
           "n_revisits": 64, "n_occupied": 8, "n_suspect": 1, "n_revisits_all": 120, "observed_s": 61.5, "revisit_max_s": 41.0, "revisit_mean_s": 14.1,
           "timing": "unknown", "threshold": {"method": {"method": "dynamic", "idle_fraction": 0.8}, "guard_db": 5.0, "rbw_correction": true},
           "threshold_db": -121.4, "guard_clamped": true, "rbw_hz": 6250.0, "obw_hz": 15000.0, "unit": "…",
           "confidence": {"lo": 0.06, "hi": 0.22, "level": "p95", "n_eff": 58.3, "independence_assumed": false},
           "revisit_biased": false, "fco_window": {"start_ns": 1789300800000000000, "end_ns": 1789301700000000000}, "subject_extent": {"f_lo_hz": 433475000.0, "f_hi_hz": 433500000.0}}],
 "coverage": {"rows": 1, "rows_with_fco": 1, "observed_s": 61.5, "unobserved_is_not_quiet": true}}
```

- **Rows** follow ADR-0012 §2.1 (`OccupancyStat`, `hk_model::attention::occupancy`) plus `subject_extent` (the subject's frequency extent in Hz). `sro` is present on band rows only. Absent optional fields are omitted.
- **`interval`:** `15m` (default) and `1h` read the persisted series (closed every 15 min of stream time; `1h` rows at hour boundaries); `span` computes one row per subject over exactly `[t0, t1]` from the history, the final channel plan and the detections (band ≤ 20 MHz, span ≤ 7 days, band × span ≤ 120 MHz·h, at most 2 M visit samples; the history is read in bounded chunks). Series closes evaluate every band observed in the interval (each dwell window and sweep hop of the observation log), so a retune inside an interval keeps both bands.
- **`bias_tee`** (T-359, additive, optional): the antenna-port bias-tee state the row was measured under — `"off"` or `"on"`, **omitted while unknown** (like `calibration`; never a null, never a bool). Omitted means no single known state can be attributed to the row: the source could not report one, the row pools frames measured under more than one, or it was written before T-359. It is never to be read as `"off"`. The bias tee powers an external LNA and is switched *during* a run, so it belongs to the measurement, not the run; baselines are keyed on it, so rows measured under different states are never compared (`/api/baselines` shows the cohorts).
  - **How "pools more than one" is decided** (T-372, refining T-359). A row states a state only when **every visit behind it** was measured under that one state, and a visit states one only when the history cells it actually read lie **wholly on one side of every bias-tee switch**. T-359 decided this per *read region*: a chunk read that was mixed anywhere made every visit in it unknown, including visits far from the switch. The provenance now carries the switch **timestamps**, so the stretches either side are attributed and only the time cell the switch landed in is lost — a 240 s read with one switch goes from 480 unknown visits to 0. What has **not** changed, and must not: a visit that **straddles** a switch states nothing, whatever the split — 9 s under one state and 1 s under the other is not a row of the longer one, since the two are different receive chains and pooling them masks a real change (T-333). Nor does a row take a majority of its visits. The refinement applies only where a **complete** switch timeline for a **single front end** can be rebuilt (the bias tee is device-local, like gain, and a step is recorded per source): a dropped step, two devices in one read, an unreported source, or a step chain that does not join up all fall back to the coarse omitted-while-unknown answer.
- **Floor and level fields** (additive, optional): `floor_db` (median floor under the thresholds; each cell is compared with its column's local floor within ±1 MHz), `floor_source` (`history` \| `eighty-percent` \| `assumed`), `floor_suspect` (the neighbourhood is mostly occupied, so the floor may be signal and `fco` low), `level_occupied_p50_db` / `level_occupied_p90_db` / `level_idle_db` (visit levels of the `fco` visits).
- **`fco`** uses activity-independent visits only (background sweep, scheduled plan; an unlogged run's own ScanPlan rows count as scheduled), time-weighted; suspect crossings (§2.6) are excluded and bounded by `fco_suspect_upper`. With fewer than 30 such visits the estimate widens to the enclosing 1 h / 6 h / 24 h window or the data span, reported in `fco_window`; `fco_all_visits` is information only and never replaces `fco`. Interactive-only observation gives `fco` absent.
- **Coverage.** Rows exist only where something was observed: a subject or interval without a row was not observed, not quiet.
- `400 invalid` for a missing or bad `f_lo`/`f_hi`/`t0`/`t1` (`f_hi > f_lo ≥ 0`, `t1 > t0`), `subject`, `interval`, `site`, or a `span` request over the limits; `500 failed` for a store error; `503 unavailable` without an occupancy engine; `405` for other methods.

`GET /api/channels` → `200`: `{"plan_version", "scheme", "f_cell_hz", "source": "learned-from-detections", "channels": [{"key": {"scheme", "lo_cell", "hi_cell"}, "source": "learned", "plan_version", "first_learned", "evidence", "obw_hz", "raster_hint"?: {"spacing_hz", "offset_hz", "source"}, "f_lo_hz", "f_hi_hz"}]}`. Keys are level-0 history cells snapped outward from the median detected extent; any change of the key set bumps `plan_version`. `400 invalid` for a bad range, `503`, `405` as above.

## Sites, baselines, candidates and score weights (T-119)

C12 baselines, novelty and the interestingness score ([ADR-0012](adr/0012-attention-memory-contracts.md) §3–§4; `crates/hk-api/src/attention.rs` over `hk_pipeline::attention::AttentionService`). Blind-first: baselines and candidates are built from measurements only; nothing here looks a frequency up. Times are Unix seconds (floats) on the stream's clock; frequencies Hz. Mutating calls need the bearer token and are audited (`site_select`, `site_update`, `baseline_refreeze`, `weights_update`). Errors: `400 invalid` (unknown field or query parameter, malformed value), `404 not_found`, `405`, `409 conflict`, `500 failed` (store I/O), `503 unavailable` (no attention service).

| Method | Path | Auth | Parameters / body | Answer |
|---|---|---|---|---|
| GET | `/api/sites` | token | – | `{"sites": [site], "current": site_key}` |
| GET | `/api/sites/current` | token | – | `{"site": site_key, "set_by": "config"\|"user"\|"gnss"\|null, "pinned", "accrues_baseline", "record": site\|null}` |
| PUT | `/api/sites/current` | token | exactly one of `{"id"}` (a known site), `{"name", "lat_deg"?, "lon_deg"?, "radius_m"?, "utc_offset_min"?}` (select by name, or create a `user` site), `{"release": true}` (unpin: fixes decide again) | as GET current; 404 unknown id |
| PUT | `/api/sites/{id}` | token | `{"name"?: string\|null, "utc_offset_min"?: integer ±840}` (at least one) | the site; 404 unknown, 409 name taken |
| GET | `/api/baselines` | token | `site`? (default current; 409 when mobile/unassigned) | `{"site", "slot", "baselines": [{"site", "cal": {"kind": "uncalibrated"\|"calibrated", "id"?}, "chain": {"kind": "unknown"}\|{"kind": "device", "id": number}, "bias_tee": "unknown"\|"off"\|"on", "scheme", "cell_factor", "subjects", "mature_subjects", "finest_resolution"\|null, "last_visit", "change_points": [{"subject", "f_lo", "f_hi", "t", "statistic": "level"\|"occupancy", "direction": 1\|-1, "cusum"}]}]}` |
| GET | `/api/baselines/slots` | token | `f_lo`, `f_hi` (required), `site`?, `slot`? (0–167 hour-of-week; default now at the site), `resolution`? (`hour-of-week`, `hour-of-day`, `day-part`, `all-hours`; default the finest mature) | `{"site", "slot", "subjects": [{"subject": {"kind": "cell", "index"}\|{"kind": "channel", "key"}, "f_lo", "f_hi", "cal", "chain": {"kind": "unknown"}\|{"kind": "device", "id": number}, "bias_tee": "unknown"\|"off"\|"on", "maturity": {"state": "mature", "resolution"}\|{"state": "immature", "observed_s"}, "mixed", "gain_states", "reference": pool, "adaptive": pool, "change_point"\|null, "refrozen_at"\|null}], "truncated"}` (at most 2 000 subjects) |
| POST | `/api/baselines/refreeze` | token | `{"site"?, "f_lo"?, "f_hi"?}` (both edges or neither) | `{"site", "refrozen": n}`: adaptive copy → frozen reference, change points cleared |
| GET | `/api/candidates` | token | `f_lo`?, `f_hi`?, `limit`? (1–1000, default 100) | `{"version", "t", "site": site_key, "weights", "candidates": [Candidate], "truncated"}`: the latest published `CandidateSet`, `score` descending. Populated on every run (T-131): with the bandit off (the default) the control thread publishes the set read-only (nothing schedules from it); with `extra.bandit` it is the bandit's provider |
| GET | `/api/attention/weights` | token | – | `{"weights": {"version", "snr", "novelty", "class_entropy", "decoder", "periodicity", "boring"}, "defaults": weights, "history": [{"version", "created", "author"}]}` (newest first; version 1 = defaults, never stored) |
| PUT | `/api/attention/weights` | token | all six weights, each in [0, 10], at least one positive term | `{"weights"}` with the next version; takes effect at the next scoring pass, never retroactively |

- **site** = `{"id", "name"|null, "lat_deg"|null, "lon_deg"|null, "radius_m", "utc_offset_min", "source": "config"|"user"|"gnss", "first_seen_ns", "last_seen_ns", "observed_s"}`; **site_key** = `{"kind": "site", "id"}`, `{"kind": "mobile"}` or `{"kind": "unassigned"}`. Only `site` keys build baselines; mobile and unassigned folds are kept as occupancy but never accrue (ADR-0012 §3.5). Occupancy rows carry the site assigned when their 15-min interval closes (T-131), so pinning the current site starts baselines, and each close steps the novelty alarms (`/api/anomalies`, the `anomalies` stream) on `hk run` and `hk serve` alike. The current assignment (site, `set_by`, `pinned`, last in-site time) is stored in the run database and restored when a service reopens it (T-136): a pinned site stays pinned with no `unassigned` gap, and a GNSS site keeps its no-fix hold on the sample clock.
- **pool** = `{"resolution", "n", "observed_s", "mean_db"|null, "std_db"|null, "fco"|null, "max_db"|null}`: the slot's pool at that resolution (levels from the most-visited gain state, occupancy over all).
- **Maturity** needs ≥ 24 h of observation in the pool; hour-of-week falls back to hour-of-day, day part, then all hours, and the resolution used is always disclosed. Immature pools give novelty 0.
- **Calibration** is part of the baseline key: a new calibration starts a new, immature baseline, so a calibration step is never novelty.
- **`bias_tee`** (T-371) names the cohort a row's numbers belong to, on both `/api/baselines` and each `/api/baselines/slots` subject row. T-333/T-359 made the antenna-port bias-tee state part of the baseline key, so **one subject at one slot can legitimately hold two pools with different numbers** — one per state. The field is what tells them apart; without it two such rows are identical on the wire and their disagreement looks like a fault rather than two cohorts. Unlike the occupancy row's `bias_tee` (which is omitted while unknown), this one is **always present and always one of `"unknown"`, `"off"`, `"on"`** — never absent, never a null, never a bool. `"unknown"` is a **cohort, not an error or a gap**: it is what every baseline stored before T-359 carries, and what any source that cannot report the state will carry indefinitely. **It must never be rendered or read as `"off"`** — nobody recording the state is not evidence the DC was absent (`hk_model::BiasTee::powered` returns `None`, which must not be `unwrap_or(false)`-ed). Rows in different cohorts are never compared, so an apparent step across a tee switch is a cohort change, not novelty.
- **`chain`** (T-381) names the receive-chain cohort a `/api/baselines/slots` row's numbers belong to, the same way `bias_tee` names the tee cohort — `/api/baselines` already carried it (T-303/T-314) and this route did not. A noise floor belongs to one front end, so a site running two front ends concurrently legitimately holds two pools per subject, one per chain; without this field those rows are identical on the wire. Rendered from the same `ChainKey` value `/api/baselines` renders for the same key, so the two routes cannot drift: `{"kind": "unknown"}` (no front end recorded — every baseline stored before T-303, and any source that never reports a `device_id`) or `{"kind": "device", "id": number}` (the hash of the front end's `Provenance::device_id`). `"unknown"` is a **cohort, not an error or a gap**, exactly as for `bias_tee`, and **must never be rendered or read as a particular device** — `ChainKey` carries no such coercion (there is no `unwrap_or`-able "default device"). Rows in different chain cohorts are never compared or pooled.
- **Candidate** is `hk_model::attention::score::Candidate` as JSON (`subject`, `freq {lo_hz, hi_hz}`, `score`, `score_norm`, `components {snr_db?, novelty, class_entropy?, decoder_available, periodicity?, boring_prior}`, `novelty {novelty, level_z?, occupancy_z?, new_emitter?, observed_s, maturity, provenance_explained}`, `suspect_fraction`, `needs_verification`, `expected_interval_s?`, `min_on_off_s?`, `next_burst_eta?` (Unix s)). `class_entropy` absent means never classified and scores as maximal uncertainty.

## Attention scheduler (T-127; ADR-0012 §5)

The scheduler of a scheduler-driven run (`hackriffd`, `hk run --schedule`, `hk replay --schedule`): tier shares, the sweep floor, the bandit and pinned leases. A run without the scheduler (`hk serve`) answers the reads with `"scheduler": null` and refuses lease changes with 409. Frequencies in Hz; times in Unix seconds on the scheduler's sample clock.

**Enabling the bandit.** Off by default. A ScanPlan enables it with `extra.bandit`: `true` for the defaults, or an object overriding `BanditConfig` fields (`ucb_c`, `discount_half_life_s`, `exploration_floor`, `sweep_floor`, `sweep_floor_window_s`, `max_arm_staleness_s`, `min_dwell_s`, `max_dwell_s`, `dwell_periods`, `prior_pseudo_dwell_s`, `arm_quantum_hz`, `max_arms`, `suspect_ban_s`, `reward`). Unknown or out-of-range fields fail the run at start. Until T-119 lands, the candidates the bandit packs come from a minimal stub scorer over confirmed tracks (novelty from first sighting, no SNR); T-119 replaces the publisher, not these routes.

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/api/scheduler[?f_lo&f_hi][&t0&t1][&tau_s]` | token | Status, leases, and POI + coverage gaps from the observation log |
| GET | `/api/scheduler/arms` | token | Bandit arm table |
| GET | `/api/scheduler/leases` | token | Active leases |
| POST | `/api/scheduler/leases` | token (header) | Create or update a lease (audited as `scheduler_lease_create`) |
| DELETE | `/api/scheduler/leases/{id}` | token (header) | Release a lease (audited as `scheduler_lease_release`) |

**`GET /api/scheduler`.** POI is computed from the observation log (T-115) with the same exact union-of-windows rule as ADR-0012 §5.5 (1 MHz cells), never from the scheduler's plan.
- `f_lo`/`f_hi` pick one region (default: the plan's regions, at most 16).
- `t0`/`t1` pick the span. POI is computed only when a span is given: without `t0`/`t1`, `poi` is empty and `span` is null, so a bare status poll never scans the observation log.
- `tau_s` is a comma-separated list of burst durations (default `0.005,0.1,1,10`; at most 16).

```json
{
  "scheduler": {
    "now": 1789300000.5, "plan_version": 1, "window_s": 600,
    "shares_s": { "discovery": 150.2, "exploit": 40.1, "explore": 8.3, "other": 0 },
    "sweep_floor": 0.25, "sweep_floor_met": true, "floor_violations": 0,
    "interactive": false, "leases": 0, "scheduled": 0, "low_power": false,
    "bandit": {
      "provider_version": 7, "arms": 12, "active_arms": 9, "pending_verifications": 0,
      "banned": 0, "total_dwell_s": 48.4, "config": { "ucb_c": 0.5, "...": "BanditConfig" },
      "counters": { "repacks": 7, "outcomes": 31, "outcomes_unmatched": 0, "exploit_dwells": 20,
        "explore_dwells": 9, "stale_forced": 0, "beacon_dwells": 2, "verifications_started": 0,
        "verifications_dropped": 0, "verifications_passed": 0, "verifications_failed": 0,
        "floor_deferrals": 3, "arms_dropped": 0, "suspect_wasted_s": 0 }
    }
  },
  "leases": [ { "id": 1, "kind": "user-pin", "center_hz": 433920000, "rate_hz": 2000000, "duration_s": null } ],
  "observation_log": true,
  "span": { "t0": 1789296400.5, "t1": 1789300000.5 },
  "poi": [ {
    "f_lo": 433000000, "f_hi": 435000000, "cell_hz": 1000000, "cells": 2, "observed_cells": 2,
    "observed_fraction": 0.31, "mean_revisit_s": 1.9,
    "poi": [ { "tau_s": 0.1, "p_poi": 0.36, "p_poi_min": 0.34 } ],
    "gap_threshold_s": 3.8,
    "gaps": [ { "f_lo": 433000000, "f_hi": 434000000, "t0": 1789296400.5, "t1": 1789296900 } ],
    "gaps_truncated": false
  } ],
  "poi_truncated": false
}
```

`bandit` is `null` when the plan does not enable it. `poi_truncated` is set when more than 200 000 records overlapped (POI then covers the first ones). Bad numbers or unpaired `f_lo`/`f_hi`, `t0`/`t1` answer 400.

**`GET /api/scheduler/arms`** returns `{ "scheduler": bool, "bandit": bool, "arms": [...] }`. Each arm: `index` (the `arm` of bandit dwell reasons), `key` (`rf_path`, `center_q`, `rate_hz`), `center_hz`, `rate_hz`, `active`, `exploration` (a hop exploration arm), `on_dc`, `prior`, `mean_reward`, `dwell_s`, `ucb` (a number, or `"inf"` for an unvisited arm without pseudo-dwell), `visits`, `staleness_s`, `suspect_fraction`, `lead` (16-hex-digit candidate key or null), `members`, `dwell_planned_s`, `required_revisit_s`, `complete_capture`, `last_reward`.

**`POST /api/scheduler/leases`** takes `{ "center_hz": 433920000, "kind": "user-pin", "duration_s": 60, "id": 3 }`.
- Only `center_hz` is required.
- `kind` is one of `user-pin` (default), `decoder`, `trunking`, `pass`, `launch`.
- `duration_s` absent means the lease holds until released.
- `id` absent assigns the next free id; the same `id` again updates that lease.
- The lease runs at the pipeline's sample rate.
- Answers 201 `{ "lease": {...} }` (200 on update); 400 for bad fields or a centre outside the device's range; 409 `no_scheduler` without a scheduler; 409 `table_full` when the lease table is full; 503 `busy` if the control thread does not answer within 2 s (the command is then cancelled and never takes effect).
- An update that changes the lease cuts its running step; an unchanged renewal, a refused create or an unknown release leaves the running step alone.

A lease preempts scheduled plans, the bandit and the sweep from the next step; the sweep floor is not enforced against it (shortfalls count as `floor_violations`).

**`DELETE /api/scheduler/leases/{id}`** answers `{ "released": id }`, 404 when no such lease is active, 400 for a non-numeric id, 409 without a scheduler, 503 `busy` as for create. The lease's unrun planned time leaves the sweep-floor window.

## Survey reports (T-121; ADR-0012 §6)

`GET /api/report?f_lo&f_hi&t0&t1[&site][&source][&format=json|csv|png]` (token): `report(region, span)`. `f_lo`/`f_hi` in Hz, `t0`/`t1` in Unix seconds on the sample clock (a replay or time-compressed scene reports its own time). `site` is `unassigned` (default), `mobile`, a site id or `unknown`. It keys the baseline comparison; `unknown` compares as `unassigned`.

**Source and site filter (T-133).** An explicitly given `site`, and `source` (16 hex digits or `unknown`), filter the report's history exactly as on `/api/history`. Without either parameter nothing is filtered: rows include every source and site, and a site report says so in `warnings`. With a filter:
- the history grid, CSV and PNG use only matching frames; cells of other origins are unobserved, not quiet;
- coverage and POI come from the filtered history tiles, because the observation log is not keyed by site or source;
- occupancy comes from the occupancy series rows of the filtered site, or from the filtered tile stand-in when the filter names a source or `unknown`;
- `warnings` states the filter, how many cells it excluded, and the tile reads by match (matched / mixed / other). The report reads its grid in chunks, so the tile counts are summed per chunk: a coarse tile read by two chunks counts twice. Cell counts are exact.

Inventory emitters and anomalies are not keyed by site or source. Schema: `hk_model::attention::report::SurveyReport` (wire structs reject unknown fields; every `Timestamp` is an integer of Unix **nanoseconds** and says so with an `_ns` name, `FreqRange` is `{lo_hz, hi_hz}`, `TimeRange` is `{start_ns, end_ns}`).

- **`format=json`** (default): the document `{schema, generated_at_ns, region, span, site, occupancy {bands, channels, truncated}, top_emitters[], change_vs_baseline {status, baseline?, resolution?, changes[]}, coverage, provenance_steps[], anomalies[], warnings[]}`. `change_vs_baseline.status` is `available` / `immature` / `no-baseline` (mobile or unassigned site, or no baselined subject) / `unavailable` (no baselines on this server); each `changes[]` entry `{subject, kind, baseline, observed, z}` combines the subject's 15-min rows, each compared against its own hour-of-week slot, with `kind` `level-above-baseline`, `busier-than-usual` (z > 0) or `quieter-than-usual` (z < 0). `generated_at_ns` is the stream time the history has reached (never the wall clock).
- **Coverage is mandatory** (`coverage {observed_fraction, observed_s, gaps[{freq, time: {start_ns, end_ns}}], gaps_truncated, never_observed[], poi[{tau_s, p_poi}], statement}`): POI for τ = 5 ms, 100 ms, 1 s, 10 s; gaps are unobserved stretches longer than twice the measured mean revisit, coalesced across adjacent frequency cells, longest first (≤ 64); `statement` always says unobserved is not quiet. Coverage comes from the observation log (T-115) when it holds visits for the box, else from history-tile coverage (a replay without the scheduler logs nothing); `warnings` names the source and, when it holds nothing for the box before some time inside the span (e.g. the log started mid-span), says that time is shown as unobserved, not quiet. A server that cannot disclose coverage (no spectrum history) answers `404` instead of a report.
- **Occupancy.** One band row for the region and channel rows (FCO descending, ≤ 64, `truncated`) over **blind** channel extents: the inventory emitters' measured extents in the box, overlapping extents merged. `fco` is the unbiased figure from activity-independent visits only (ADR-0012 §2.5) and is never substituted: it is absent when the source cannot give it. Until the occupancy engine (T-118) lands, rows come from history-tile occupancy (floor + the pyramid margin), which mixes activity-driven dwells, so rows carry **no `fco`**: `fco_all_visits` = occupied grid rows / observed grid rows, `n_revisits_all` = observed grid rows (`n_revisits`/`n_occupied` 0), `revisit_biased: true`, `threshold.method: history-tile` with the pyramid `margin_db`, `fbo` = coverage-weighted tile occupancy, `timing: unknown`; channel rows sort by `fco`, then `fco_all_visits`; `warnings` says so.
- **Top emitters** (≤ 20, most sightings in the span first): `emitter_id`, measured `freq`, `first_seen_ns`/`last_seen_ns`, `sightings` in the span, `lifecycle` (`candidate`/`confirmed`), channel `fco` and `fco_all_visits` (each copied from its channel row, so no `fco` from the tile stand-in), `top_suggestion` (the top-ranked explanation's service label: a suggestion, never truth) and `new_in_span`.
- **Change vs baseline.** `status` is `unavailable` until baselines (T-119) land; `changes` is non-empty only when `available`. A warning states that no comparison is implied.
- **Provenance steps** (time order): `{t, kind, freq?, detail}` with `kind` `gain` (LNA/VGA/amp or gain table), `calibration`, `spur-mask`, `antenna-port`, `bias-tee`, `sample-drop`, … from the history tiles' provenance, e.g. `detail: "lna 32→24 dB"`. Overload share and mixed calibration appear in `warnings`.
  - `bias-tee` (T-332) is an antenna-port bias-tee change, `detail: "bias tee off→on"` over the three states of `BiasTee` (T-325). The DC powers an external LNA, so the floor moves the instant it arrives: a switch is a **self-inflicted** change the operator made, and the alarm path explains the coincident anomaly with it — the anomaly is still recorded, naming the switch as its cause, never silently dropped. A transition **to or from `unknown`** is listed too (the frames either side are not comparable) but is a change of *knowledge*, not necessarily of the DC, so it never accounts for a change on its own: it is written beside the alarm as a possible contributor and the alarm still raises. A step is listed only when the two states differ.
- **`format=csv`** (`text/csv; charset=utf-8`): `#` comment lines carrying the coverage statement, observed fraction, POI and baseline status; then `row,f_lo_hz,f_hi_hz,t0_s,t1_s,fco,fco_all_visits,fbo,n_revisits,n_occupied,n_revisits_all,observed_s,revisit_biased` (`fco` empty when unavailable) with `band` and `channel` rows, then `gap` and `never_observed` rows.
- **`format=png`** (`image/png`): tile-occupancy heatmap over the report's own history grid (time down, frequency right; one pixel per cell, the same grid the JSON/CSV use), unobserved cells grey with a diagonal hatch.

**Grid budget.** The report reads the finest history level within 4096 time rows × 1024 frequency columns and 500 000 cells (the `/api/history` `MAX_API_CELLS` budget), else the top level if it fits 500 000 cells; a box larger than that even at the top level is `400`. The grid is read in ≤ 256-row chunks, each under its own short history lock, so a report never locks ingest out for its whole build; e.g. 48 h × 20 MHz on the default ladder is 192 × 800 cells (15 min × 25 kHz).

Errors: `400` (bad region/span, a box over the grid budget, `site` or `format`), `404` (no coverage source), `405` (not GET), `500` (store failure), `503` (no report service on this server). A bad `source` is `400` as well.

## Anomalies and novelty alarms (T-122; ADR-0012 §7–§8)

Every anomaly the run recorded (noise-floor episodes and C12 novelty alarms), with ranked explanations. Alarms are raised blind from baseline novelty (level above baseline, new emitter, busier than usual, **quieter than usual**, change point) with hysteresis (raise after 2 scored intervals ≥ 0.7, clear after 3 < 0.4; a re-raise within 1 h re-opens the same anomaly). Busier and quieter than usual also accumulate evidence over consecutive same-direction intervals (ADR-0012 §7.2): a sparse-visit channel no single interval can alarm raises once its run is improbable enough, and its `unexplained` explanation then carries `sequential_z`, `sequential_intervals` and `sequential_novelty` evidence values. Alarms are suppressed while the site is mobile or unassigned, a device provenance step explains the change, or the baseline is immature. **Explain the device first:** a gain/calibration/spur-mask/antenna/overload/restart/drop/site step that fits the change is stored as an anomaly whose top explanation is `self-inflicted` (alarm state `explained`, status `resolved`), never as a novelty alarm. Other alarms get the C30 explanations (cached external events) and an `unexplained` explanation scored `1 − best external score`. Times are Unix seconds on the sample clock; frequencies in Hz.

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/anomalies?[f_lo&f_hi][&t0&t1][&kind][&status][&cursor][&limit]` | Newest first. `kind` (the anomaly kind): `new-emitter`, `busier-than-baseline`, `quieter-than-baseline`, `noise-floor-rise`, `novelty`, `level-above-baseline`, `change-point`. The alarm key's `kind` names two of these differently: alarm `busier-than-usual` is anomaly `busier-than-baseline`, and alarm `quieter-than-usual` is anomaly `quieter-than-baseline`; filter by the anomaly name. `status`: `open`, `resolved`, `dismissed`; `limit` default 100, max 1000 |
| GET | `/api/anomalies/{id}` | One anomaly with every current explanation (best first) and `history` |
| POST | `/api/anomalies/{id}/dismiss` | `{"note"?}`: dismiss a novelty alarm; its key is suppressed for 7 days of sample time, then must re-raise (audited) |
| POST | `/api/anomalies/{id}/reopen` | `{}`: lift a dismissal, or re-open a cleared alarm (audited) |

- **List:** `{anomalies[], next_cursor, truncated, suppressions}`. `suppressions` is `{<alarm kind>: {<mobile-site / unassigned-site / provenance-explained / immature-baseline / dismissed>: count}}` since the service started.
- **Anomaly:** `{id, kind, subject, f_lo, f_hi, t0, t1, t, score, baseline_ref, detector_version, status, alarm, explanations[], history[]}`. Lists and stream messages carry the top 3 explanations and no `history`. `explanations[]` is `{id, cause, correlation_type, score, provisional, rule_version, t, evidence}`; `cause.kind` is `external-event`, `emitter`, `own-history`, `self-inflicted` (with `reason`) or `unexplained`. `history[]` is `{status, t, note}` with notes `raised`, `cleared`, `reopened`, `reopened-by-user`, `dismissed;until_ns=…[;note=…]`, `explained`, `self-inflicted`.
- **`alarm`** (novelty alarms; `null` otherwise): `{key {kind, site, subject}, state (open, cleared, dismissed, explained), last_transition (raised, held, reopened, cleared, dismissed, undismissed, explained), raised_at, last_t, reopen_count, cleared_at, dismissed_until, f_lo, f_hi, detail, explained_step_t?}`. `explained_step_t` (explained rows only) is when the explaining device step happened. `f_lo`/`f_hi` is the hull while open (the anomaly row keeps the extent at raise). `detail` is `AlarmDetail` (`observed`, `baseline_mean`, `baseline_spread`, `z`, `novelty`, `intervals_above`, `observed_s`, `unit`, `cal`, `resolution`, `slot`, `stages_applied`).
- **New emitter (T-136):** fed from the inventory's first sightings at each occupancy close. First sightings in the last hour at the current site are grouped by frequency (extents within the alarm cell-merge gap, 200 kHz). Every emitter carries its group's new-emitter novelty for that hour: the Poisson tail of the group's own first sightings against the count the site's usual rate expects in the hour, so an ordinary sighting beside a burst does not inherit the burst's count. `subject` is the cells covering the new emitters' measured extents, merged like other cell alarms, so a burst on one channel is one alarm. `detail.observed` is the group's first sightings in the hour, and `baseline_mean` the count expected at the site's usual rate. One new emitter on its own is rarely novel, except under the persistent single-emitter rule (T-138, ADR-0012 §7.1): on a mature discrete site whose usual rate makes any new emitter in an hour unlikely (P ≤ α ≈ 0.011), a new emitter seen with clean detections in two consecutive closes (or again on the next visit) scores as two first sightings and raises; `detail.observed` is then 1. No field changes. While the rate is immature (< 24 h observed at the site) each new emitter is counted once as `immature-baseline`.
- **Stream `anomalies`** (ADR-0004 `messages`, schema `hackriff.anomaly/1`, metadata only): one message per transition (`raised`, `held`, `reopened`, `cleared`, `explained`, `dismissed`) with `metadata {kind: "anomaly", transition, anomaly}` in the list row shape.

Errors: `400` (bad id, region, span, kind, status, cursor/limit or body field), `404` (no such anomaly), `405`, `409` (dismissing a floor episode or a self-inflicted anomaly; reopening an open alarm), `500`, `503` (no anomaly service).

## Trunking load index (T-273, AWARE-067; C23)

A derived view over the [`GrantEvent`](07-data-model.md) stream (`hk_model::trunking::GrantEvent`, C23, T-266/T-269/T-270): how busy a trunked system's control channel is, from channel-grant **metadata** alone — "count control-channel grants per talkgroup category as a live incident-activity indicator, without recording audio" (AWARE-067). It is computed on demand from the stored event stream over the requested window, not accumulated: the response is a snapshot, never a running counter that grows without bound.

**Metadata only, and this is the hard boundary the route exists to keep.** The index counts `grant`/`grant-update`/`call-start` events and the distinct talkgroup identifiers they named; it never reads `CallRecord` (the call-content aggregate) and never carries an audio, vocoder, payload or bit field. A talkgroup identifier is metadata of the same kind as a channel number, not content.

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/trunking/load?t0&t1[&system]` | The load index over `[t0, t1)` (Unix seconds), for every known trunk system, or just `system` (a `TrunkSystemId` UUID) when given |

- **Response:** `{window: {t0, t1}, systems: [{system, grants, distinct_talkgroups, grants_per_min}]}`. `grants` counts `grant`/`grant-update`/`call-start` [`GrantKind`](07-data-model.md) events in the window (`denied`, `outside-window` and `unmapped-channel` are logged elsewhere per C23 but are not channel grants; `call-end` closes traffic already counted at its `grant`/`call-start`); `distinct_talkgroups` counts the distinct talkgroup identifiers those events named; `grants_per_min` is `grants` normalised by the window's length. A system with no rows for the window answers `grants: 0` (unobserved and quiet are not distinguished here — this route is a busy/quiet index, not a coverage map).
- Reads up to 20,000 grant rows per system per request (`MAX_GRANTS_PER_QUERY`), bounding the query even though the underlying event stream is append-only forever.

Errors: `400 invalid` (missing/non-numeric `t0`/`t1`, `t1 <= t0`, or `system` not a UUID), `405` for other methods, `500 failed` for a store error, `503 unavailable` without a trunking store.

## Attention and memory (planned, M2; ADR-0012)

**Planned, not served yet.** None of the routes below are in `ROUTES` today (the observation log's, occupancy's, T-119's sites, baselines, candidates and weights, the attention scheduler's, the survey report's and T-122's anomalies have landed and moved to their own sections above). They are named here so the parallel M2 tasks and the M2 UI hooks (T-123) code against one surface. When an owning task lands, it moves its rows into a normal section with request/response shapes and contract tests.
- **Contracts:** [ADR-0012](adr/0012-attention-memory-contracts.md).
- **Schemas:** `hk_model::attention` (observation records, `OccupancyStat`, baselines, `CandidateSet`, `SurveyReport`, alarm detail).

Conventions:
- **Units.** Frequencies in Hz; times in Unix seconds (floats); results capped in size with a `truncated` flag.
- **Blind-first.** Candidates, channels and reports are computed from measurements; database suggestions appear only as labelled suggestions. Nothing here tunes to a database frequency.
- **Coverage.** Every route that reports occupancy or absence also returns coverage and POI (ADR-0012 §5.5, §6.2). Unobserved is never reported as quiet.
- **Audit.** Mutating routes are audited like every other.
- **Content.** Streams are metadata only.

No planned routes remain in this section.

Streams: the `observations` stream (T-115) and the `anomalies` stream (T-122) are served; see "Observation log" and "Anomalies and novelty alarms" above.

## UI decision logic moved server-side (T-079)

The user's direction (2026-09-14): the web UI will be rewritten later as a one-screen exploratory UI; until then, **the backend owns all signal logic — recognition, analysis, classification, demodulation, decoding — and the UI is a thin client over this document**, so it can be replaced without backend changes. `GET /api/analysis/strongest` (above) is the first move under that rule: picking the strongest signal in a frequency range is spectrum *analysis*, not presentation, so it moved out of `ui/src/listen.ts` (`peakBinIndex`/`strongestInView`, which inspected a raw client-held FFT row) into the backend, which can look at its own measured spectrum history instead of one row the browser happened to have decoded. The UI toolbar (`ui/src/listen.ts` `installListen`) now polls this endpoint roughly once a second and caches the answer, so choosing a Listen target still runs synchronously inside the click handler (required to unlock audio playback on mobile browsers) rather than awaiting a fetch.

Target-priority arithmetic (a click beats a selection beats the strongest-in-view; a click is boxed ±25 kHz and clamped to the current view; a selection is clamped to the 1 MHz Listen span) stayed client-side: it resolves already-known UI state (what was clicked, which selection is active, the current view bounds) rather than measuring anything about the signal itself, and the server independently enforces the 1 MHz Listen span regardless (`MAX_LISTEN_SPAN_HZ`, `hk_stream::audio::ListenTarget`). See `ui/src/listen.ts` for the full reasoning and `crates/hk-api/tests/http_api.rs` (`analysis_strongest_*`) / `crates/hk-cli/tests/api_contract.rs` for the tests.

## One shared time axis (T-337)

**The rule, from the user** (CLAUDE.md, "Time, the waterfall, and the live view", invariant 1): *for the current view there is a single canonical mapping between absolute capture time and screen position, and everything time-varying is laid out through it and moves together — waterfall rows, every signal box, selections, the time cursor, the scrubber playhead. Overlays are anchored in capture time, never at fixed screen coordinates: a box must sit on, and scroll with, the exact waterfall energy it describes.* The user states the consequence too: **signal boxes drifting out of step with the waterfall's rows-per-second is a violation of this invariant, not a cosmetic bug.**

The division of labour is the same one [T-334](#span-matched-resolution-t-334) drew for resolution. **Mapping a time to a pixel is presentation** and lives in the client, like the pixel↔Hz axis mapping. **Deciding what time a record has is not**, and lives here.

### The backend's obligation

**Every time-varying record this document serves carries the absolute capture time the client must place it at.** Not an index, not a position in a sequence, not something to be worked out from when the response arrived. Concretely:

| Record | Where its time comes from |
|---|---|
| Spectrum rows (the waterfall) | the binary record header's `t` (i64 ns, Unix epoch) **and** `sample_index` — [stream contract §5.2](stream-contract.md). `t` is the timestamp of the row's **first** element, and it is the capture clock: `Δt` between rows equals `Δsample_index / bandwidth_hz`. |
| History cells | `t0_s` + `k·t_cell_s` — contract, not inference ([span-matched resolution](#span-matched-resolution-t-334)) |
| Presence extents, events, boxes | `t_start_s`, `t_end_s`, `duration_s` — and `duration_s` is computed here, so a client never derives a timespan from two fields it was handed |
| Inventory rows | `first_seen_s`/`last_seen_s` (a hull, never an extent) plus `presence.last_interval` (the extent); each sub-object carries its own `t_s` |
| Selections | `t_lo`/`t_hi`, or `null` for a deliberately timeless "this band, any time" region |
| Floor steps | per step `t_s` + `duration_s` |
| The strongest-signal box | `window.t0_s`/`t1_s` and the cell's own `t_start_s`/`t_end_s` ([above](#get-apianalysisstrongest--strongest-signal-in-a-band-t-079)) |
| Listen, taps, frames, IQ | the binary record header's `t` + `sample_index`, as for spectrum rows |
| Stored records served verbatim | occupancy `interval`/`fco_window`, observation `span`/`planned`/`observed`, `SignatureMatch`, `Classification`, `refined`, the site record, `SurveyReport` — Unix **nanoseconds**, every field named `…_ns` per the [units convention](#conventions) |

### A declared rate is not a clock

The spectrum stream header's `sample_rate_hz` is the **row rate the producer declares**. It describes how fast rows are *produced*; it does not say where the rows on a screen sit, and it must never be used to place a row or an overlay in time. Two independent reasons, both live today:

- On a content-forbidding class the declared rate is deliberately **above** the actual row rate — `RowPlan::declared_hz = min(row_rate_hz × 1.1, 50)` (`hk_pipeline::class`) — because the egress gate's token bucket needs headroom. A client dividing an age by it thinks rows are 10 % closer together than they are.
- Rows are not evenly spaced in capture time anyway. A gated row, a dropped run, or a frame the client skipped under backlog advances capture time without advancing the waterfall's ring.

Both errors grow **linearly with age**, so a box drawn from a declared rate walks down the screen away from its own energy — exactly the failure the user names. The only correct mapping is the one built from the rows' own timestamps, which is why every row carries one.

### Consequences for a client

- Keep each row's `t` with the row, and invert *that* to place an overlay (`ui/src/axis.ts` `rowsBackAt`, `ui/src/waterfall.ts`). A row's `t` is its **first** sample, so row *k* covers `[t(k), t(k−1))` and an emission filling exactly row *k* lands on exactly row *k*.
- One mapping per view, shared by rows, presence boxes, selections, the drag draft and the time scale. A selection dragged over rows 1–4 and then drawn back must land on rows 1–4.
- Never fabricate a placement for a record that has scrolled off the rows held: draw nothing rather than a box clamped to a height it never had.

### Contract tests

`crates/hk-cli/tests/api_contract.rs` asserts the spectrum stream's record timestamps **by value**: absolute Unix-epoch nanoseconds (not an offset or a counter), monotonic, and `Δt == Δsample_index / bandwidth_hz`, with the declared row rate only bounding the observed one. `analysis_strongest_*` asserts the window and cell times by value against an explicit `now`. `inventory_and_analysis_strongest_find_the_blind_fm_station` asserts the row's `measured` block by value (T-350): its levels equal the flat `snr_db`/`peak_dbfs` exactly, its extent lies inside the row's own first/last-seen hull and is never wider than it (a hull is not a measurement), `duration_s` is `t_end_s − t_start_s` and is bounded to seconds, and the block is null exactly when the levels are. The units sweep `every_serialized_time_declares_its_unit` already holds its three `_s` fields to the seconds law, so no parallel unit test was added. The client-side invariant — a box's placement is a pure function of its capture time under the same mapping the rows use — is tested in `ui/test/app-centre.test.ts` and `ui/test/axis.test.ts` over deliberately uneven row clocks and a 10 %-fast declared rate.

### Known gaps (named, not fixed here)

- **Nanoseconds still reach the wire where an hk-model struct is serialized directly** — `/api/occupancy` `interval`/`fco_window`, `/api/observations` record `span`/`planned`/`observed`, `SignatureMatch`, `Classification`, `refined`, the site record and the whole of `SurveyReport` — but they are no longer *silent* about it (T-349): every such field now ends in `_ns` and the units convention above states the law, so a field carrying nanoseconds beside one carrying seconds is legible rather than a 31-year trap. What has **not** been done is converting them, which would mean a second schema beside the stored one; the reasons are in the convention. The one field that was outside the law — stream records' `t`, nanoseconds declared by the stream contract rather than by its name — is now inside it: T-354 renamed it `t_ns` in stream contract 1.2, so nothing this API serves carries an absolute time under a name that misdeclares its unit.
- **Scheduler leases carry `duration_s` with no start or expiry** (T-351; the `/api/status` half of this gap is fixed, above — `t`). A lease's remaining time is not derivable from what `GET /api/scheduler`/`GET /api/scheduler/leases` serve, which is exactly the inference the shared-time-axis invariant forbids. Fixing it means serving an absolute expiry the served `Lease` does not carry today: `hk_core::scheduler::core::Scheduler` computes one internally (`ActiveLease::until`, set from `now + duration_ns` when the lease is added) but `Scheduler::leases()` strips it back down to the bare request-shaped `Lease` before hk-pipeline's `SchedulerView` and hk-api's `lease_json` ever see it — so the fix is a shape change to `hk_core::scheduler::bandit::tiers::Lease`/`Scheduler::leases()` and to how `crates/hk-pipeline/src/control.rs` builds `SchedulerView`, not an hk-api-only rename.

## Listen as an audio pipeline (planned, MAUTO; ADR-0011 §8, ADR-0015 §12)

**Planned, not served yet. Nothing in this section changes any route today.** T-221 is a design task: it records what Listen becomes once audio is an ordinary recipe output, so the UI and external clients can see the intended end state. No row below is in `ROUTES`; when an owning task lands it moves its rows into a normal section with request/response shapes and contract tests (T-079).

- **Contracts:** [ADR-0011 §8](adr/0011-decoder-workbench-contracts.md) (the `audio_out` sink block, the `audio` output kind, `input.liveness`, `refine.objective.builtin`), [ADR-0015 §12](adr/0015-decoder-synthesis-contracts.md) (the chooser, ownership, the staged migration).

**What does not change, at any stage:**

- `GET /ws/open/listen?emitter=|detection=|f_lo=&f_hi=` and TCP `open/listen?…` keep their names, parameters and refusal codes. There is still **no `mode` parameter** — mode and every other parameter stay estimated.
- The audio profile (`docs/stream-contract.md` §12.2) keeps `kind: "audio"`, `datatype: "ri16_le"`, `sample_rate_hz: 48000`, `frame_samples: 960`, its type-1 data records and its type-3 status keys (`level_dbfs`, `snr_db`, `squelch_open`, `agc_gain_db`, `frames`, `latency_ms`, …). A closed squelch stays a jump in `sample_index` plus `DISCONTINUITY`.
- Audio stays **content**: the pre-attach gate (`listen_class`) still runs before any ring read, and the egress gate still withholds payloads under a class that forbids content.
- `/api/status` keeps reporting `listen.budget`: an audio pipeline counts as a listener, not only as a chain.
- `GET /api/analysis/strongest` is unrelated (it picks a target) and unchanged.

**Planned deltas, all additive:**

| Method | Path | Planned delta |
|---|---|---|
| GET | `/ws/open/listen` | Implementation only: chooser → ephemeral audio pipeline → that pipeline's `audio` output. Gains an optional `pipeline=<id>` form that **attaches** to an already-running audio pipeline instead of starting one. |
| GET | `/ws/{stream_id}` | A running audio pipeline also offers its output as the always-on stream `audio/<pipeline>/<output>`, beside `inspector/<pipeline>/<output>`. |
| POST | `/api/pipelines` | Accepts a recipe with an `audio` output; the start runs the pre-attach content gate and is admitted against the listener budget. |
| GET | `/api/pipelines/{id}` | `outputs[]` gains `kind: "audio"`; session-owned pipelines report `owner: "session"`. |
| — | audio stream header | The `audio` object gains `pipeline_id`, `recipe` (`<id>@<version>`), `output_id`, `edit_rev`. Existing keys keep their names and meanings; unknown-key-tolerant readers are unaffected. |
| — | audio status records | Keep every audio key and additionally carry the per-node `<node>.<metric>` batch (ADR-0011 §1.3) on the same tick. |
| POST | `/api/outputs/record/start` | Unchanged in this plan (`kinds: ["audio"]` still opens its own chain). |

**Staging.** The switch is flag-gated (`HK_LISTEN_PIPELINE=1`, default off) before it is defaulted on, and only for the modes that have blocks (WFM/NBFM/AM). USB/LSB/CW have no blocks and keep the existing chain. See ADR-0015 §12.9 for the numbered stages and what is observable after each.

## Saved measurements (T-818, MAP-18)

A measurement a researcher takes on the canvas — Δf, Δt, bandwidth, duration, symbol rate or period — kept as a durable **object with value + unit + place + time + provenance**, not a readout that vanishes on mouse-up ([docs/25 §4](25-spectrum-research-workflow.md), the normative store contract in §10, [ADR-0023](adr/0023-map-ui-and-research-state.md); RESEARCH-003). Stored in the run's user-metadata database beside bookmarks and selections, so they survive a restart. Errors: `{"error", "code"}` with `invalid`, `not_found`, `conflict`, `unavailable`; `405` with `Allow` for a wrong method; `401` before dispatch.

| Method | Path | Body / query | Response |
|---|---|---|---|
| GET | `/api/measurements` | `collection`? (UUID); `f_lo`, `f_hi` (Hz), `t0`, `t1` (capture-clock Unix s) — **all four or none**; `limit`? (default 500, max 2000); `cursor`? | `{"window", "collection", "measurements": [Measurement, …], "count", "matched", "limit", "next_cursor"}` |
| POST | `/api/measurements` | `{"kind", "cursors": [{"f_hz", "t_s"}, {"f_hz", "t_s"}], "n"?, "note"?, "collection_id"?, "id"?, "view"}` | `Measurement` (`201`); `409 conflict` when `id` already exists |
| GET | `/api/measurements/{id}` | – | `Measurement` |
| PUT | `/api/measurements/{id}` | `{"cursors"?, "n"?, "note"?, "collection_id"?, "view"?}` (`null` clears `n`/`note`/`collection_id`); **re-computed server-side** | `Measurement` |
| DELETE | `/api/measurements/{id}` | – | `{"deleted": Measurement}` |

`Measurement`: `{id, collection_id, kind, value, unit, basis, f_lo_hz, f_hi_hz, t0_s, t1_s, cursors: [{f_hz, t_s}, …], n, note, provenance, created_s, updated_s}`, with `provenance` the same stamp as an annotation's: `{device_id, center_hz, span_hz, sample_rate_hz, t_capture: [t0_s, t1_s], tier ("live-iq"|"spectrum-history"|"survey-overview"), authored_s, actor, authored: true}`.

- **Cursors in, value out** (docs/25 §10.4). The body carries the place — exactly two cursors, each a capture-clock point `(f_hz, t_s)` — and the server computes `value`, `unit` and the place (`f_lo_hz..f_hi_hz` = the cursors' frequency hull, `t0_s..t1_s` their time hull; cursor order does not matter). A body carrying `value`, `unit`, `basis`, `f_lo_hz`, `f_hi_hz`, `t0_s` or `t1_s` is `400 invalid`, on POST and PUT alike, and a PUT that moves a cursor or changes `n` is re-measured. There is no path by which a stored value is one the client computed; the client's live drag readout is ephemeral presentation arithmetic over its own pixel↔(Hz, s) maps.
- **Kinds and units.** `delta_f` = `f_hi − f_lo` (`Hz`); `delta_t` = `t1 − t0` (`s`); `bandwidth` (`Hz`) and `duration` (`s`) are the same spans but must be **positive**; `symbol_rate` = `n / (t1 − t0)` (`Bd`) and `period` = `(t1 − t0) / n` (`s`), the inspectrum reciprocal pair, where `n` (1–1 000 000, **required** for these two and refused for the others) is the number of cycles/symbols the cursors span.
- **`basis` says what the value is a function of.** Today it is always `"cursors"`: the span the two cursors mark (for `bandwidth`, the user-marked width docs/25 §4 allows). It is **not** a −3 dB width or an estimated symbol rate over the IQ under the place; a data-derived measurement would carry a different `basis`, never silently replace this one.
- **The list is durable but bounded.** Unwindowed it returns every measurement, paged (the `/api/events` contract: `count` this page, `matched` the whole filter, `next_cursor` an opaque offset string, `null` on the last page); `f_lo`/`f_hi`/`t0`/`t1` narrow it to places intersecting that closed box, and `collection` to one MAP-17 collection (not checked for existence). Order: newest capture time first (`t1_s`, then `t0_s`, then id).
- **Provenance is stamped by the server; the client sends only `view`** — `{center_hz, span_hz, t_capture: [t0_s, t1_s], tier, device_id?}`, exactly as for annotations. The server adds `actor` (the token fingerprint `tok-…`, **never the token**), `authored_s` (wall clock — when the human acted, never compared with the capture-clock cursors or `t_capture`), `authored: true`, and `sample_rate_hz` **only** when this run holds the device `device_id` names. `provenance`, `author`, `actor`, `authored`, `authored_s`, `created_s` or `updated_s` in a body (top level or inside `view`) is `400 invalid`. A PUT without `view` keeps the original stamp; one with `view` re-stamps it.
- **Audited** as `measurement_create`, `measurement_update`, `measurement_delete` (token id, peer, request, old/new, status), with **no `device` key**: measuring is a view act and reaches no radio. Without an audit log every mutating route is `503 unavailable`; without a store, every route is.
- **Never detection input.** A saved measurement mints no candidate, moves no threshold and never pre-populates the inventory (docs/25 §10.7).

## Reserved: the map-UI research routes (MMAP, T-800 / ADR-0023)

**These routes are SPECIFIED and RESERVED, not yet served.** T-800 (MAP-00) fixed their shapes so the
four stores are four instances of one pattern rather than four designs; each owning ticket lands its
route **and** its contract test in the same change (T-079). Until then a request to one of these paths
answers `404 no such endpoint` **after** the ordinary `401` auth check — asserted by
`api_contract.rs::mmap_research_routes_are_reserved_and_gated`, which also pins that they stay
token-gated once they exist. Full contracts: [`docs/25 §10`](25-spectrum-research-workflow.md) (the
four stores), [`docs/24 §7`](24-canvas-as-data-surface.md) (priors), [ADR-0023](adr/0023-map-ui-and-research-state.md).

| Method | Path | Ticket | Body / query | Answer |
|---|---|---|---|---|
| GET | `/api/annotations` | MAP-16 | **served (T-816)** — see [Annotations](#annotations-t-816-map-16) | `{annotations, count, matched, next_cursor}` |
| POST | `/api/annotations` | MAP-16 | **served (T-816)** — `{kind ("text"\|"box"\|"marker"), f_lo_hz, f_hi_hz, t0_s, t1_s, label, body?, collection_id?, view}` | `Annotation` (201, audited) |
| GET/PUT/DELETE | `/api/annotations/{id}` | MAP-16 | **served (T-816)** — any create field on PUT | `Annotation` / `{deleted}` |
| GET | `/api/collections` | MAP-17 | `limit`? (500, max 2000), `cursor`? | `{collections, count, matched, next_cursor}` |
| POST | `/api/collections` | MAP-17 | `{name, note?, color?}` | `Collection` (201, audited) |
| GET/PUT/DELETE | `/api/collections/{id}` | MAP-17 | any create field; `{visible}` toggles | `Collection` / `{deleted, members_deleted}` |
| GET/POST | `/api/collections/{id}/markers` | MAP-17 | `{name, f_center_hz, bandwidth_hz?, t_center_s?, duration_s?, note?, view}` | `{markers, …}` / `Marker` (201, audited) |
| GET/PUT/DELETE | `/api/markers/{id}` | MAP-17 | any create field (`null` clears optionals) | `Marker` / `{deleted}` |
| GET | `/api/measurements` | MAP-18 | **served (T-818)** — see [Saved measurements](#saved-measurements-t-818-map-18); `collection`?, `f_lo`/`f_hi`/`t0`/`t1`?, `limit`?, `cursor`? | `{measurements, …}` |
| POST | `/api/measurements` | MAP-18 | **served (T-818)** — `{kind, cursors, n?, note?, collection_id?, view}` — **`value`/`unit` in the body is `400 invalid`** | `Measurement` (201, audited) |
| GET/PUT/DELETE | `/api/measurements/{id}` | MAP-18 | **served (T-818)** — `{cursors?, n?, note?, collection_id?, view?}`; a moved cursor is **re-computed server-side** | `Measurement` / `{deleted}` |
| GET/POST | `/api/views` | MAP-19 | `{name, note?, center_f_hz, span_f_hz, center_t_s?, span_t_s?, follow_live, pane_layout?}` | `{views, …}` / `SavedView` (201, audited) |
| GET/PUT/DELETE | `/api/views/{id}` | MAP-19 | any create field | `SavedView` / `{deleted}` |
| GET | `/api/priors` | MAP-12 | `f_lo`,`f_hi`,`t0`,`t1` | `{priors: [{f_lo_hz, f_hi_hz, service, allocation, source, rank, reason, off_raster_hz?}]}` — ranked **explanations**, computed on demand, gated like `/api/events` |

**Rules every one of them inherits** (`docs/25 §10`):

- **Provenance is stamped by the server.** The client sends only `view` — the view context it was on
  (`center_hz`, `span_hz`, `t_capture`, `tier`, `device_id`?). The server adds `actor` (a token
  fingerprint, never the token), `authored_s` (wall clock) and `authored: true`, and keeps
  `t_capture` (capture clock) and `authored_s` strictly apart. A request supplying a server-owned
  provenance field is `400 invalid`.
- **One paging contract**, the `/api/events` one. "Durable" is not "unbounded".
- **Audited like `/api/bookmarks*`/`/api/selections*`**, and `503 unavailable` for every mutating
  endpoint when there is no audit log.
- **No entry ever carries a `device` key.** Authoring is a view act and reaches no radio; the one
  research act that may command it is *restoring* a saved view whose frequency lies outside the tuned
  window, which uses the ordinary gated retune offer (see "Device actions").
- **`/api/bookmarks` is not replaced.** It becomes a compatibility facade over one reserved,
  un-deletable collection; the bookmark rows and the collection's frequency-only markers are the same
  rows.
- **Nothing in these stores feeds blind detection** — on create or on import. They mint no candidate,
  set no family and never pre-populate the inventory.

## Route table completeness

`crates/hk-api/src/http.rs::ROUTES` is the single source of truth for what answers under `/api/` and `/ws/`; `crates/hk-cli/tests/api_contract.rs::every_route_in_the_route_table_is_documented` asserts every entry in it appears (method and path together) somewhere in this file, so this document cannot silently fall behind the server.

## Sources

- [ADR-0002](adr/0002-ui-web-vs-native.md) (the UI is the first client of the same API external programs use)
- [`docs/stream-contract.md`](stream-contract.md) — the versioned wire contract every stream and the TCP handshake follow
- [`docs/07` §2.11](07-data-model.md) (emitter lifecycle), [`docs/07` §2.20](07-data-model.md) (selections)
- `crates/hk-api/src/http.rs`, `control.rs`, `selections.rs`, `outputs.rs`, `inventory.rs`, `query.rs`, `ondemand.rs`, `bridge.rs`, `tcp.rs`, `auth.rs`
- `crates/hk-cli/src/serve.rs`, `pipeline.rs` (how `hk serve` composes the pipeline and this API)
