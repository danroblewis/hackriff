# API reference

**Status:** Engineering (T-050, T-051, T-052, T-060, T-061, T-067, T-078, T-079). Code: `crates/hk-api/src/http.rs` (`ROUTES`, the complete table — nothing else answers `2xx` under `/api/` or `/ws/`), `control.rs` (device/display/recording/bookmarks), `selections.rs`, `outputs.rs`, `inventory.rs` (T-078 lifecycle), `query.rs` (read-only history/floor/inventory/analysis), `ondemand.rs` (`/ws/open/*`), `bridge.rs` (`/ws/<id>`, discovery), `tcp.rs` (the TCP stream server). Contract tests: `crates/hk-cli/tests/api_contract.rs` (drives a real `hk serve` over the mock SDR device, T-049, and asserts every route's status, JSON shape and auth refusals), `crates/hk-cli/tests/control_http.rs`, and per-module tests under `crates/hk-api/tests/`.

**The web UI is a thin client over this document** (ADR-0002): the UI, `hk`, and any other program talk to the exact same HTTP/WS/TCP surface. Nothing here is UI-only. Streamed payloads (framing, headers, binary record layout, content-class gating) are the versioned wire contract in [`docs/stream-contract.md`](stream-contract.md); this document covers the plain HTTP/WS/TCP *endpoints* (status codes, request/response JSON) and links to the stream contract wherever a route opens or discovers a stream.

## Conventions

- **Base URL.** `hk serve` binds `127.0.0.1:<port>` by default (printed at start as `http://<addr>/#token=<token>`) and prints the TCP stream server's address alongside it. Binding a non-loopback address exposes every route below to anyone who can reach that interface; there is no TLS in M0 (the cloudflared tunnel the user runs separately adds TLS).
- **Units.** Frequencies are Hz; times are Unix seconds as JSON numbers (floats) everywhere in this document, so browsers never handle `i64` nanoseconds. (Stream *records* use `i64` nanoseconds — see the stream contract.)
- **Auth (bearer token).** Every `/api/*` and `/ws/*` request needs the server's token (`Token::verify`, constant-time comparison; a missing/wrong/expired token is `401` before anything about streams, the device or an id is revealed):
  - `Authorization: Bearer <token>` works everywhere.
  - `?token=<token>` works **only for `GET` requests** (browsers can't set headers on a `WebSocket` connection, so `/ws/*` and any other read needs this form to be usable from a page). A mutating request (`POST`/`PUT`/`DELETE`) carrying `?token=` instead of the header is refused `401` with a message saying so — the token must never land in a mutating URL (proxy logs, browser history).
  - `hk serve` keeps the token in a `0600` file (`hk_api::default_token_path`, `$HK_TOKEN_FILE` or `$XDG_CONFIG_HOME/hackriff/api-token`) or `$HK_TOKEN`; the UI reads it from the URL fragment (`#token=`, never sent to the server) and keeps it in `sessionStorage` for the tab.
- **CORS.** No `Access-Control-Allow-*` header is ever sent. `OPTIONS` preflights always answer `403` (so a cross-origin page cannot ride a CORS grant to smuggle the token or a JSON body). A mutating request whose `Origin` names a different host than `Host` (or `X-Forwarded-Host`, trusted only from a loopback peer, i.e. the local cloudflared tunnel) also answers `403`. Same-origin use — the UI served by this same server, directly or through the tunnel — is unaffected.
- **Methods.** `GET`, `POST`, `PUT`, `DELETE`, `OPTIONS` are parsed; anything else is `405`. A *known* path with the wrong method is `405` with an `Allow` header listing the methods it does accept. An *unknown* `/api/*` path is `404`.
- **Errors.** Every non-2xx JSON body is `{"error": "<message>"}`; every route reached through the control dispatcher (`control.rs`/`selections.rs`/`outputs.rs`/`inventory.rs` — everything except the five read-only endpoints in the first table below and `/ws/*`) additionally carries a stable machine `"code"`: `{"error", "code"}`. Error messages never echo raw request values. Common codes: `invalid` (400, malformed/out-of-range field), `not_found` (404), `unauthorized` (401), `forbidden`/cross-origin (403), `not_live` (409, device settings on a replayed recording), `conflict` (409, a re-plumb or another operation is in progress), `refused` (409, legal/content-class gate said no), `finished` (409, the run has ended), `timeout` (504), `out_of_range` (400, a device value outside its capabilities), `unsupported` (501, the device lacks the capability, e.g. no bias tee), `busy`/`quota` (503/507, output-recording admission), `unavailable` (503, the server has no audit log / bookmark store / output recorder / etc. for this feature).
- **Audit.** Every **mutating** request to `/api/control/*`, `/api/bookmarks*`, `/api/selections*`, `/api/outputs*` or `/api/inventory/{id}*` is written to the run's audit log (`<data dir>/control-audit.jsonl`, mode `0600`) once authenticated: time, token id (never the token), peer, method, path, action name, request body, old/new values, status, result. Unauthenticated mutating attempts are logged too, coalesced per client to bound disk use. **`GET` requests are never audited**, on any route. Without an audit log every mutating endpoint answers `503 unavailable`. See `crates/hk-api/src/control.rs` module docs for the exact schema.
- **Bounded resources.** At most `ServerConfig::max_connections` (default 64) connection threads at once (WebSocket consumers included); request heads ≤ 16 KiB, bodies ≤ 64 KiB, both within `request_timeout` (default 10 s); `/api/history`/`/api/floor`/`/api/inventory` cap result size (below).
- **Receive only.** No route reaches a transmit path; `transmit.available` is always `false` (C37 stays gated at the type level, not just by convention — there is no transmit operation to call).

## Read-only query routes

| Method | Path | Auth | Query | Response | Errors |
|---|---|---|---|---|---|
| GET | `/api/streams` | token | – | Discovery document (T-060, below) | 401 |
| GET | `/api/history` | token | `f_lo`, `f_hi` (Hz), `t0`, `t1` (Unix s), `max_cells`? (default 100 000, max 500 000) | T-017 region-over-time grid (below) | 400 invalid region/cells, 404 no history store on this server |
| GET | `/api/floor` | token | `f_lo`, `f_hi`, `t0`, `t1`, `max_steps`? (default 1024, max 20 000) | T-021 calibrated floor-vs-time series (below) | 400, 404 no floor product |
| GET | `/api/inventory` | token | see below | T-018/T-078 signal inventory, one page (below) | 400 invalid filter, 404 no inventory store |
| GET | `/api/inventory/{id}` | token | – | One inventory entry (T-078, same shape as a list row) | 404 not_found, 503 unavailable |
| GET | `/api/analysis/strongest` | token | `f_lo`, `f_hi` (Hz), `window_s`? (default 5, max 300) | T-079 strongest observed signal in the band over the recent window (below) | 400 invalid region/window, 404 no history store |
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
      "fft_size": 1024, "open_consumers": 0,
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
    { "name": "symbols", "ws_path": "/ws/open/symbols", "tcp_target": "open/symbols", "kind": "symbols", "...": "…" }
  ],
  "tcp": { "addr": "127.0.0.1:8788",
           "handshake": "<tcp_target>?token=<token>[&param=value...]\\n",
           "refusal": "one frame {\"type\":\"refused\",\"status\",\"code\",\"reason\"} instead of the header" }
}
```

`tcp` is `null` when no TCP stream server runs. See [Streams](#streams-websocket-tcp-and-on-demand-openers) below and `docs/stream-contract.md` §10/§12/§13 for what each named stream/opener actually carries.

### `GET /api/history` — region-over-time grid (T-017, AWARE-042)

A `nt × nf` grid (row-major, time then frequency) of the finest pyramid level whose cell count fits `max_cells`, over `[f_lo, f_hi) × [t0, t1)`. Unobserved cells are `null` — *not observed* is not *quiet* (C26).

```jsonc
{
  "level": 0, "unit": "dbfs-per-hz", "f_cell_hz": 3125.0, "f_lo_hz": 99600000.0, "nf": 384,
  "t_cell_s": 0.1, "t0_s": 1789300800.0, "nt": 1200, "percentiles": [10.0, 90.0],
  "max_db": [-71.2, null, "…"], "mean_db": ["…"], "p_low_db": ["…"], "p_high_db": ["…"],
  "occupancy": ["…"], "occupancy_max": ["…"], "coverage": ["…"], "frames": ["…"],
  "provenance": { "frames": 12000, "suspect_fraction": 0.0, "dropped_samples": 0, "gain_changes": 0,
                  "gain_states": ["…"], "calibration": null, "calibration_mixed": false,
                  "first_frame_s": 1789300800.0, "last_frame_s": 1789300920.0 },
  "tiles_read": 4
}
```

A region too large for the cell budget even at the coarsest level is `400`.

### `GET /api/floor` — calibrated floor vs time (T-021, SPACE-050)

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

### `GET /api/inventory` — signal inventory (T-018, T-078, AWARE-053/AWARE-042)

Query parameters (all optional, combined with AND): `f_lo`&`f_hi` (Hz, given together), `t0`&`t1` (Unix s, given together), `state` (comma-separated `candidate`/`confirmed`/`deleted`; **default: candidate and confirmed — deleted entries are listed only when `deleted` is explicitly asked for**), `status` (comma-separated `known`/`unexpected-here`/`unknown`), `tag`, `scheme` (identity scheme), `family`, `cursor` (row offset, ≤ 1 000 000), `limit` (default 100, max 500).

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
      "f_center_hz": 101300000.0, "bandwidth_hz": 150000.0, "f_lo_hz": 101225000.0, "f_hi_hz": 101375000.0,
      "first_seen_s": 1789300800.0, "last_seen_s": 1789300920.0, "count": 42,
      "known_status": "known",
      "status": { "status": "known", "author": "prior", "t_s": 1789300810.0,
                  "reason": "on FM broadcast allocation", "prior_ref": "band-plan/us-fm@1", "reason_withheld": false },
      "tags": [], "tags_withheld": false, "family": "wfm-broadcast",
      "classification": { "family": "wfm-broadcast", "confidence": 0.9, "open_set_score": 0.1,
                           "model_version": "…", "t_s": 1789300820.0 },
      "classifications": 3,
      "identity_scheme": "rds-pi", "identity_class": "unrestricted", "withheld": false,
      "identity_value": "A1B2"
    }
  ],
  "next_cursor": null, "limit": 100, "identity_access": "standard"
}
```

`identity_value` is present only when the row's identity is in clear (`withheld: false`); on a withheld row a status/lifecycle reason from an author who may have seen the identity is itself withheld (`reason_withheld: true`, `reason: null`). Never included: decode content, fingerprints, links. No frequency lookup ever runs before detection — the inventory is populated purely from blind measurement (vision step 4); the band-plan/licence database only supplies `explanations` and `status`, ranked, never a starting point.

**Lifecycle (T-078).** Every emitter starts `candidate`. An auto rule (e.g. a continuous trust-confirmed track, or a valid decode/identity) or a user promotes it to `confirmed`; a user (or nothing) can delete either. `deleted` is final for that row — it leaves the default list and entity resolution, but its detections, tracks, links and history are kept (visible with `state=deleted`); a later sighting of the same signal creates a *new* candidate. See `docs/07` §2.11.

### `/api/inventory/{id}` — one entry, promote, delete (T-078)

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/api/inventory/{id}` | – | One entry (same row shape as a list entry above; deleted entries included) |
| POST | `/api/inventory/{id}/promote` | `{"reason"?}` | `{"changed", "entry"}` — candidate → confirmed; `changed: false` when already confirmed |
| DELETE | `/api/inventory/{id}` | `{"reason"?}` | `{"deleted": entry}` |

`{id}` may be the id of an entity that has since been merged into another (the API resolves to the live emitter). `reason` (optional on the mutating routes) is a free-text string of up to `LIFECYCLE_TEXT_MAX` bytes; both actions are audited (`inventory_promote`, `inventory_delete`) with the old/new lifecycle state and the token fingerprint as actor. Errors: `404 not_found` (unknown id, or an entry already deleted), `400 invalid` (unknown body field, bad `reason`), `503 unavailable` (no inventory store or no audit log).

### `GET /api/analysis/strongest` — strongest signal in a band (T-079)

Backend replacement for client-side peak-picking over a locally held spectrum row (see [UI decision logic moved server-side](#ui-decision-logic-moved-server-side-t-079) below): the strongest observed signal (max-hold, dB/Hz) in `[f_lo, f_hi)` over the last `window_s` seconds, read from the same spectrum-history pyramid as `/api/history`.

```jsonc
{ "found": true, "f_center_hz": 101300000.0, "f_lo_hz": 101200000.0, "f_hi_hz": 101400000.0, "max_db": -71.2 }
```

or `{"found": false}` when nothing was observed in the window. Unlike a live FFT row, spectrum-history cells carry no per-bin skirt to fit a box to, so the reported box is a fixed **±100 kHz** around the strongest cell's centre, clamped to `[f_lo, f_hi)` — not a measured signal bandwidth. `400` when `f_hi <= f_lo`, `f_lo`/`f_hi` are out of range, or `window_s` is not a finite number in `(0, 300]`.

### `GET /api/status` — pipeline counters (T-027)

Opaque, per-build JSON object of counters (source samples, chain stats, control-loop stats under `"control"`, listen/chain admission under `"listen"`/`"budget"` when the pipeline exposes them, …). Never content, never an identity. `404` when this server has no pipeline status function attached (e.g. a bare bridge with no composed pipeline).

**Compute providers (T-056, ADR-0007)** are reported under `"compute"`. They are chosen once per run and never change mid-run.

| Field | Type | Meaning |
|---|---|---|
| `compute.options` | object | Options in force after `--compute` and the `HK_COMPUTE*` environment: `provider` (`auto`, `cpu`, `cpu-mt`, `accelerate`, `gpu` or `cuda`), optional `stft`/`pfb` overrides, `threads`, `gpu_in_flight` |
| `compute.stft.<reader>` | object | For each always-on reader (`detect`, `history`, `spectrum`): `requested`, `provider` (the one used), `backend` (e.g. `cpu-rustfft`), `fft_len`, and `fallback` (why the requested provider was refused, or `null`) |
| `compute.providers[]` | array | Each provider's `name`, `compiled`, `conformant`, `usable` (`null` when not probed) and `detail` (a description, or why the provider is unusable) |
| `compute.stft_builds` | number | STFTs built this run (one per reader per segment, plus display rebuilds) |
| `compute.provider_changes` | number | Builds whose provider differed from that reader's first. Always `0` in a correct run |

## Control API (T-050)

Device, display, recording and bookmark endpoints, all behind the bearer token, all audited once authenticated. Every mutating body is a JSON object (`Content-Type: application/json`); an unknown field is `400 invalid`. Device endpoints (`center`, `rate`, `gains`, `bias_tee`) act on a *live* source ([`ApiState::live_control`]); on a replayed recording they answer `409 not_live`, while display, pause, recording and bookmarks keep working.

| Method | Path | Body | Response | Notable errors |
|---|---|---|---|---|
| GET | `/api/control/state` | – | `{live, device, tuning, run, display_limits, transmit: {available: false, reason}, audit, routes}` | – |
| POST | `/api/control/center` | `{"center_hz"}` | `{tuning, run}` | 400 invalid/out_of_range, 409 not_live/conflict/finished |
| POST | `/api/control/rate` | `{"sample_rate_hz"}` | `{tuning, run}` | as above |
| POST | `/api/control/gains` | `{"gains": {"<stage>": <dB>, …}}` | `{tuning}` (gains quantised per stage) | 400, 409 not_live |
| POST | `/api/control/bias_tee` | `{"enabled"}` | `{tuning}` | 501 unsupported (no bias tee), 409 not_live |
| POST | `/api/control/baseband_filter` (T-067) | `{"bandwidth_hz"}` | `{tuning}` (validated against `device.baseband_filter`) | 501 unsupported (no selectable filter), 400 out_of_range, 409 not_live |
| POST | `/api/control/display` | any of `{"fft_size", "averaging", "rows_per_s", "window"}` (T-067; at least one) | `{display}` | 400 invalid |
| POST | `/api/control/pause` | `{}` (or empty) | `{display}` | – |
| POST | `/api/control/resume` | `{}` (or empty) | `{display}` | – |
| POST | `/api/control/record/start` | `{"label"?, "max_s"?}` | `{recording}` | 409 refused (a content-forbidding class), conflict (already recording) |
| POST | `/api/control/record/stop` | `{}` (or empty) | `{recording}` (the stored `Recording`) | – |
| GET | `/api/bookmarks` | – | `{"bookmarks": [Bookmark, …]}` | – |
| POST | `/api/bookmarks` | `{"name", "f_center_hz", "kind"?, "bandwidth_hz"?, "note"?}` | `Bookmark` (`201`) | 400 invalid |
| GET | `/api/bookmarks/{id}` | – | `Bookmark` | 404 not_found |
| PUT | `/api/bookmarks/{id}` | any create field (`null` clears `bandwidth_hz`/`note`); `{"name"}` alone renames it | `Bookmark` | 400, 404 |
| DELETE | `/api/bookmarks/{id}` | – | `{"deleted": Bookmark}` | 404 |

`Bookmark`: `{id, kind ("marker"|"bookmark"), name, f_center_hz, bandwidth_hz, note, created_s, updated_s}`.

`tuning`: `{center_hz, sample_rate_hz, gains: {"<stage>": <dB>, …}, bias_tee, baseband_filter_hz}` (`bias_tee`/`baseband_filter_hz`: `null` without that capability, or before it's been set explicitly — the device's own default is in force). `display`: `{fft_size, averaging, rows_per_s, paused, window}` (`window`: `"hann"`, `"blackman-harris"` or `"flat-top"`; T-067). `run`: `{live, content_class, content_permitted, center_hz, sample_rate_hz, segment, replumbing, finished, display, recording}` — `segment` increments on every re-plumb (a retune or rate change into a window of another content class). `recording`: `{active, id, label, center_hz, sample_rate_hz, samples, lost_samples, max_s, stored, ended}`.

**`display_limits` (T-067)** reports the bounds `POST /api/control/display` accepts, so the UI stops hard-coding hk-pipeline's `DISPLAY_*` constants: `{fft_size_min, fft_size_max, averaging_max, rows_per_s_min, rows_per_s_max, windows}` — `fft_size` must be a power of two in `[fft_size_min, fft_size_max]`, `averaging` in `[1, averaging_max]`, `rows_per_s` in `[rows_per_s_min, rows_per_s_max]`, `window` one of the `windows` list. `null` when this server has no running pipeline (503-class servers only; both live and replayed runs report it).

`device.baseband_filter` (inside the `caps_json`, T-067) describes the selectable bandwidths: `{"min_hz", "max_hz"}` for a continuous range or `{"values_hz": [...]}` for a discrete list (e.g. the HackRF's MAX2837 filter steps), or `null` when the device has no selectable filter.

**Legal gating lives in the pipeline and repository, not the API**: a retune re-derives the window's content class and re-plumbs at a block boundary; recordings are refused under a class that forbids content; the API never opens content itself (bookmarks are user metadata only). `transmit.available` is always `false`.

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

A client may choose `id` (any UUID) so an optimistic/offline-created selection keeps its identity when it syncs; a retried create then answers `409` and the client should `PUT` instead. `Selection`: `{id, name, f_lo, f_hi, t_lo, t_hi (null = any time), notes, tags, links: [{kind, target, t, note}] (oldest first), created, updated}`.

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

## Streams: WebSocket, TCP and on-demand openers

Full framing, header fields, binary record layout, drop markers, backpressure and `content_class` egress gating are the versioned wire contract: **[`docs/stream-contract.md`](stream-contract.md)**. This section covers only the HTTP/WS-level *endpoints* that open or discover a stream.

| Method | Path | Auth | Response |
|---|---|---|---|
| GET | `/ws/{stream_id}` | token (header or `?token=`) | Upgrades to WebSocket and bridges the named always-on stream (§10) |
| GET | `/ws/open/{name}` | token | Upgrades and opens an on-demand stream (§12): `listen`, `bits`, `symbols`, with query parameters per opener |

**`GET /ws/{stream_id}`** (e.g. `spectrum/live`): the header JSON is the first **text** message, verbatim; every later record is one message — text (NDJSON line) for `messages` streams, binary (32-byte record header + payload) for every binary kind. Refusals never upgrade the connection and are plain HTTP: `401` (bad/missing token, checked before the upgrade), `403` (a `own-key-decrypted` stream — those are Unix-socket-only and never served over the bridge), `404` (unknown `stream_id`), `410` (stream finished), `426` (not a valid WebSocket upgrade request), `503` (consumer cap reached).

**`GET /ws/open/{name}?<params>`**: e.g. `listen?emitter=<id>` or `listen?f_lo=<Hz>&f_hi=<Hz>` (mode and parameters are always estimated — there is no `mode` parameter), `bits`/`symbols` (optionally `emitter=`/`detection=`/`f_lo=&f_hi=`). Unlike `/ws/{id}`, a **refusal completes the upgrade** (browsers cannot read an HTTP error body on a failed upgrade): one text message `{"type": "refused", "status", "code", "reason", "content_class"?}`, then the socket closes with code `4000 + status` (e.g. `4403` a legal/class refusal, `4404` unknown opener/target, `4409` outside the tuned window or mid-replumb, `4503` at the listener/chain/CPU budget). A refusal never carries content. On success the connection is bridged exactly like `/ws/{stream_id}` above (header text message, then records) as a **remote** consumer, so an `own-key-decrypted` target is refused the same way. **Listen** additionally streams periodic **status** records (binary, type 3: `level_dbfs`, `snr_db`, `squelch_open`, `agc_gain_db`, `frames`, `latency_ms`, …).

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
- **Audit.** Mutating routes are audited (`recipe_save`, `recipe_delete`, `pipeline_start`, `pipeline_edit`, `pipeline_save`, `pipeline_stop`) with ids and revisions, not whole documents. `POST /api/recipes/validate` saves nothing: it needs the token in the header like every POST but is not audited.

| Method | Path | Body | Answers |
|---|---|---|---|
| GET | `/api/blocks` | – | `{"blocks": [BlockDescriptor]}`: `name`, `version`, `group`, `doc`, `inputs`/`outputs` (`{name, types, diagnostic}`), `params` (`{name, type, required, default, hot, doc}`), `params_pinned` |
| GET | `/api/recipes` | – | `{"recipes": [{id, name, version (latest), versions, builtin, builtin_version, description, match, input: {port}}]}` by id |
| POST | `/api/recipes` | recipe document | 201 `{id, version, warnings, recipe}`: saved as `latest + 1` after validation against `/api/blocks` |
| POST | `/api/recipes/validate` | recipe document | 200 `{valid, errors, warnings, edges: [{node, port, from ("input" \| "node.port"), type}]}`. Nothing is saved. |
| GET | `/api/recipes/{id}` | – | the latest version's document; 404 `not_found` |
| GET | `/api/recipes/{id}/versions/{version}` | – | that version's document |
| DELETE | `/api/recipes/{id}` | – | `{id, deleted_versions}`: every saved (user) version; 409 `conflict` when only a built-in exists |
| POST | `/api/pipelines` | `{recipe_id, version?, target}` or `{recipe, target}` (an unsaved draft) | 201 the pipeline (below). Refusals: 400 `invalid` (with paths), 404 unknown recipe/target, 409 `outside_window` (the channel is not inside the tuned window), 422 `unrealisable` / `unsupported_input` (non-`iq` input, `follow-hops` until T-093, a capture target until T-092), 503 `busy` (chain budget; running chains untouched), 503 `unavailable` (re-plumbing), 410 `source_ended` |
| GET | `/api/pipelines` | – | `{"pipelines": [pipeline]}` |
| GET | `/api/pipelines/{id}` | – | the pipeline |
| PUT | `/api/pipelines/{id}/recipe` | draft recipe document (same `id`) | 200 `{id, edit_rev, applied_at_sample, plan, swap: {rebuilt, reset, updated, kept}, warnings}`. An invalid draft is 400 and the running revision is untouched; 409 `ended`; 504 `timeout` (no chunk boundary within 10 s; nothing changed) |
| POST | `/api/pipelines/{id}/save` | – | 201 `{id, version, pipeline_id}`: the running revision saved as the recipe's next version |
| DELETE | `/api/pipelines/{id}` | – | `{"stopped": pipeline}`: the pipeline stops, its streams finish and `/api/streams` no longer lists them |

**Pipeline** JSON: `id` (`p<n>`), `recipe_id`, `recipe_version`, `edit_rev` (0 = as started), `state` (`running` \| `ended`), `end_reason` (`stopped`, `source-ended`, `segment-ended`, `retune: …`, `rate-change: …`, `error: node <id>: …`), `target`, `channel: {center_hz, bandwidth_hz, sample_rate_hz}`, `content_class`, `emitter_id`, `started` (Unix s), `nodes: [{id, block, outputs}]` (topological order), `outputs: [{id, kind (inspector \| stage), stream_id}]`, `status` (the latest status tick: flat `<node>.<metric>` keys, ADR-0011 §1.3), `stats: {samples, chunks, frames, gaps, discontinuities, skipped_samples, edits, status_ticks}`, `warnings`.

**Hot edit** (ADR-0011 §2.3). The draft is a whole recipe document. The server plans it against the running revision (`plan.nodes[]`: `{id, change: unchanged | params-hot | params-cold | rebuilt | added | removed, keys?}`, `plan.reset`, `plan.field_maps_changed`, `plan.input_changed`, `plan.outputs_changed`). It builds new instances off the pipeline thread, which swaps graphs at its next chunk boundary.
- Unchanged nodes keep their state. Hot parameters, field-map content included, apply in place. Nodes downstream of a rebuilt node are reset.
- The ring reader never moves, so no sample is lost and capture never pauses.
- An `input` edit re-plumbs the channel DDC and rebuilds every node.
- Unchanged outputs keep their streams and consumers. Changed outputs get new streams, and removed ones finish.
- An `edit` record marks the boundary on every inspector stream, and later frames carry the new `edit_rev`. Edits don't save: use `POST /api/pipelines/{id}/save`.

**Streams of a pipeline** (stream contract §14; discovery lists them under `/api/streams`):

| Stream | How to open | Carries |
|---|---|---|
| `inspector/<pipeline>/<output>` | `GET /ws/inspector/<pipeline>/<output>` (the `/ws/{stream_id}` route), TCP `inspector/<pipeline>/<output>?token=…`, or the opener `GET /ws/open/inspector?pipeline=<id>[&output=<id>]` / TCP `open/inspector?…` (first inspector output by default) | messages stream, `message_schema: hackriff.inspector/1`: one frame record per frame, one `status` record per ~250 ms tick (every node batched), one `edit` record per applied edit |
| `stage/<pipeline>/<output>` | `/ws/stage/<pipeline>/<output>` or TCP (a recipe's declared `stage` outputs) | §14.4 binary records, one per processed chunk |
| on-demand stage tap | `GET /ws/open/stage?pipeline=<id>&node=<node>[&port=<port>][&view=raw]` / TCP `open/stage?…` | any node port: `iq` → `iq`/`cf32_le`, `real` → `audio`/`rf32_le`, `soft` → `symbols`/`rf32_le`, `bits` → `bits`/`ru8`, `frames` → frame records. The tap costs nothing until opened and stops when the consumer leaves. 404 unknown pipeline/node/port, 410 ended, 422 `view=spectrum` (not served yet) |

## Decoder workbench (planned, M1; ADR-0011)

**Planned, not served yet.** None of these routes are in `ROUTES` today (the served T-088 and T-089 routes moved to "Recipes and pipelines" and "Inspector" above). They are named here so the parallel M1 tasks and the inspector UI (T-090) code against one surface. When an owning task lands, it moves its rows into a normal section with request/response shapes and contract tests.
- **Contracts:** [ADR-0011](adr/0011-decoder-workbench-contracts.md).
- **Schemas:** `hk_recipe` (recipe, field map, block descriptor types).
- **Wire formats:** [`docs/stream-contract.md` §14](stream-contract.md) (inspector frame records, stage streams, recorded decoded streams).

Conventions:
- **Bodies and errors.** Recipe and field-map bodies are the JSON documents themselves, within the 64 KiB body cap. A validation failure is `400 invalid` with `errors: [{path, message}]` and `warnings`. Messages never echo values.
- **Times.** Unix seconds (floats), as elsewhere in this document.
- **Audit.** Mutating routes are audited like every other.
- **Matching.** Recipes are ranked against *measured* signal parameters; nothing tunes to a recipe's frequency hints or starts a recipe unasked.

| Method | Path | Owner | Purpose |
|---|---|---|---|
| GET | `/api/recipes/match` | T-088 | `?emitter=<id>`: recipes ranked against the emitter's measured family, bandwidth, symbol rate, burstiness and features, with reasons |
| POST | `/api/assist/sync` | T-091 | Sync-word and period suggestions over a capture's frames or bits, scored |
| POST | `/api/assist/fields` | T-091 | Entropy-based field-boundary suggestions as field-map fragments, scored |
| POST | `/api/assist/crc` | T-091 | CRC/BCH parameter search over a capture's frames → `crc`/`bch` parameter objects, scored |

Assist suggestions are never applied automatically: the user accepts or edits them into a recipe, which then goes through `POST /api/recipes/validate`.

## Decoded captures (T-092, M1; ADR-0011 §4, stream contract §14.7)

**Every running pipeline's inspector output is recorded automatically** ("always recorded", docs/13 layer 4), so a parser can be authored and re-run over what was actually decoded. Recording, index and quota live in `hk_store::decoded`; `hk_pipeline::recipes::capture` tees each inspector stream in when a pipeline starts or a hot edit adds an output; these routes only route, audit and shape errors (`crates/hk-api/src/captures.rs`).

- **Storage** (`<data dir>/captures/`). `<id>.hks` is the §3 byte stream itself: the header frame, then the records as published. Frame records are stored without `content.layers` (derived; re-parse recomputes them). `<id>.idx` is the frame index: one 16-byte little-endian entry per frame record, `u64` byte offset + `i64` `t` (Unix ns), so frame and time scrubs seek. `<id>.json` is the catalogue entry.
- **Never blocks capture.** The recorder is a local consumer of the stream's publisher, with a bounded queue (4 MiB) and its own writer thread. A slow disk makes the publisher drop records; drops are counted in `dropped_records`, and the pipeline never waits.
- **Content rule.** The §6 gate runs before storage. A frame whose class forbids content is stored metadata-only and can never be re-parsed.
- **Quota.** Defaults: 1 GiB total (`$HK_DECODED_CAPTURE_TOTAL_BYTES`) and 64 MiB per capture (`$HK_DECODED_CAPTURE_BYTES`, clamped to at most a quarter of the total).
  - At the per-capture size a recording **rolls** to a new capture: a new id, `segment + 1` and the same header.
  - Past the total, the **oldest finished captures are evicted first**. If only recording captures remain, their segments roll and new records are dropped and counted until the store is back under quota.
  - A capture that ends with no frame records is deleted.
  - When `hk serve` starts, captures a previous process left recording are closed with `end_reason: "interrupted"`.

| Method | Path | Auth | Answers |
|---|---|---|---|
| GET | `/api/captures` | token | `{"captures": [Capture]}`, newest first |
| GET | `/api/captures/{id}` | token | `Capture`; 404 `not_found` |
| DELETE | `/api/captures/{id}` | token (audited `capture_delete`) | `{"deleted": Capture}`; 409 `conflict` while it is still recording; 404 `not_found` |
| GET | `/api/captures/{id}/frames` | token | `?[from_frame=<n> \| from_t=<unix s>][&to_t=<unix s>][&limit=1..500, default 100]` → `{capture_id, capture, stream, total_frames, from_frame, limit, next_from_frame, frames}` |

**`Capture`** fields:
- `id` (`[A-Za-z0-9_.:-]{1,128}`), `pipeline_id`, `recipe_id`, `recipe_version` (at the capture's start; frames carry their own `metadata.recipe_version`/`edit_rev`), `output_id`, `stream_id`, `content_class`, `segment`.
- `started`, `ended` (`null` while recording), `t_first`, `t_last` (first and last frame `t`), all Unix s.
- `frames` (frame records stored), `bytes` (stream bytes stored), `dropped_records`, `recording`.
- `end_reason`: `finished`, `rolled`, `slow-consumer`, `write-failed`, `drain-timeout`, `detached` or `interrupted`.

**Scrubbing** (`/frames`):
- `from_t` resolves through the index to the first frame at or after that time. The response's `from_frame` says which frame that is, so a time scrub is also a frame position.
- `to_t` ends the page at the first frame after it (`next_from_frame: null`). Give `from_frame` or `from_t`, not both.
- `frames` are the stored frame records in order, served fail closed like the parse route: a record whose class (or the stream's) forbids content, or that is `own-key-decrypted`, is `gated: true` without `content`.
- `stream` is as in the parse route, with `inspector.source: {kind: "capture", capture_id, reparse: false}`.

**Re-parse** with a draft field map is `POST /api/captures/{id}/parse` ("Inspector" above). It uses the index too: without a field map only the requested page is read; with one, the fit pass reads the first 100 000 frames and a page past them is a second seek.

**Replay stream**: `GET /ws/open/inspector?capture=<id>[&from_frame=<n>][&field_map=<recipe_id>@<version>:<map_id>]` (TCP `open/inspector?capture=…&token=…`).
- It replays the stored frame records from frame `n` (an index seek). With `field_map` (a saved recipe version's map), each record is re-parsed: `content.layers` and `metadata.fit` are added.
- The header is the recording's, with `stream_id: capture/<id>` and `inspector.source: {kind: "capture", capture_id, reparse}`. The §6 gate runs again.
- `seq` is the replay stream's own (from 0, for its drop detection). `t`, `metadata` (`frame`, `sample_index`, `recipe_version`, `edit_rev`, ...) and `content` are the recording's.
- Records are paced to the consumer rather than dropped. The stream finishes after the last frame stored when it opened. Only frame records are replayed (not `status`/`edit`).
- Refusals: 400 `bad-request` (bad `capture`, `from_frame` or `field_map` syntax), 404 `not-found` (capture, recipe version or map), 422 `unreadable`/`invalid`.

Errors are `{"error", "code"}`: `400 invalid` (bad id or query, messages never echo values), `404 not_found`, `405` (with `Allow`), `409 conflict`, `422 unreadable` (not a readable inspector stream), `500 unreadable` (store I/O), `503 unavailable` (no capture store on this server).

## UI decision logic moved server-side (T-079)

The user's direction (2026-09-14): the web UI will be rewritten later as a one-screen exploratory UI; until then, **the backend owns all signal logic — recognition, analysis, classification, demodulation, decoding — and the UI is a thin client over this document**, so it can be replaced without backend changes. `GET /api/analysis/strongest` (above) is the first move under that rule: picking the strongest signal in a frequency range is spectrum *analysis*, not presentation, so it moved out of `ui/src/listen.ts` (`peakBinIndex`/`strongestInView`, which inspected a raw client-held FFT row) into the backend, which can look at its own measured spectrum history instead of one row the browser happened to have decoded. The UI toolbar (`ui/src/listen.ts` `installListen`) now polls this endpoint roughly once a second and caches the answer, so choosing a Listen target still runs synchronously inside the click handler (required to unlock audio playback on mobile browsers) rather than awaiting a fetch.

Target-priority arithmetic (a click beats a selection beats the strongest-in-view; a click is boxed ±25 kHz and clamped to the current view; a selection is clamped to the 1 MHz Listen span) stayed client-side: it resolves already-known UI state (what was clicked, which selection is active, the current view bounds) rather than measuring anything about the signal itself, and the server independently enforces the 1 MHz Listen span regardless (`MAX_LISTEN_SPAN_HZ`, `hk_stream::audio::ListenTarget`). See `ui/src/listen.ts` for the full reasoning and `crates/hk-api/tests/http_api.rs` (`analysis_strongest_*`) / `crates/hk-cli/tests/api_contract.rs` for the tests.

## Route table completeness

`crates/hk-api/src/http.rs::ROUTES` is the single source of truth for what answers under `/api/` and `/ws/`; `crates/hk-cli/tests/api_contract.rs::every_route_in_the_route_table_is_documented` asserts every entry in it appears (method and path together) somewhere in this file, so this document cannot silently fall behind the server.

## Sources

- [ADR-0002](adr/0002-ui-web-vs-native.md) (the UI is the first client of the same API external programs use)
- [`docs/stream-contract.md`](stream-contract.md) — the versioned wire contract every stream and the TCP handshake follow
- [`docs/07` §2.11](07-data-model.md) (emitter lifecycle), [`docs/07` §2.20](07-data-model.md) (selections)
- `crates/hk-api/src/http.rs`, `control.rs`, `selections.rs`, `outputs.rs`, `inventory.rs`, `query.rs`, `ondemand.rs`, `bridge.rs`, `tcp.rs`, `auth.rs`
- `crates/hk-cli/src/serve.rs`, `pipeline.rs` (how `hk serve` composes the pipeline and this API)
