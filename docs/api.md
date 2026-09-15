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

## Decoder workbench (planned, M1; ADR-0011)

**Planned, not served yet.** None of these routes are in `ROUTES` today. They are named here so the parallel M1 tasks and the inspector UI (T-090) code against one surface. When an owning task lands, it moves its rows into a normal section with request/response shapes and contract tests.
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
| GET | `/api/blocks` | T-088 | Block catalogue: every `BlockDescriptor` (ports, parameter schemas with `hot` flags, docs) |
| GET | `/api/recipes` | T-088 | Recipes: `id`, `name`, latest `version`, all versions, `match` hints, built-in or user |
| GET | `/api/recipes/{id}` | T-088 | The latest version, or `?version=<n>` |
| POST | `/api/recipes` | T-088 | Save a recipe as a new version (`latest + 1`; saved versions are immutable) |
| POST | `/api/recipes/validate` | T-088 | Validate without saving: `{valid, errors, warnings, edges}` |
| GET | `/api/recipes/match` | T-088 | `?emitter=<id>`: recipes ranked against the emitter's measured family, bandwidth, symbol rate, burstiness and features, with reasons |
| POST | `/api/pipelines` | T-088 | Run `{recipe_id, version?, target: {emitter_id} \| {selection_id} \| {band: {f_lo, f_hi}} \| {capture_id}}` → the pipeline (`503 busy` at the chain budget) |
| GET | `/api/pipelines` | T-088 | Running pipelines with their revision, per-node status and output stream ids |
| GET | `/api/pipelines/{id}` | T-088 | One pipeline |
| PUT | `/api/pipelines/{id}/recipe` | T-088 | Hot edit: the draft recipe document → `{edit_rev, plan, applied_at_sample}`. Capture never stops; an invalid draft leaves the running revision untouched. |
| POST | `/api/pipelines/{id}/save` | T-088 | Save the running revision as the recipe's next version |
| DELETE | `/api/pipelines/{id}` | T-088 | Stop a pipeline (its streams finish) |
| GET | `/api/captures` | T-092 | Recorded decoded streams: pipeline, recipe revision, output, span, frames, bytes |
| GET | `/api/captures/{id}/frames` | T-092 | `?from_frame&limit` (≤ 500): stored frame records |
| POST | `/api/captures/{id}/parse` | T-089 | Re-parse `{field_map, from_frame?, limit?}` → frames with layer trees + `fit` summary (ok/partial/failed, errors by path) |
| POST | `/api/assist/sync` | T-091 | Sync-word and period suggestions over a capture's frames or bits, scored |
| POST | `/api/assist/fields` | T-091 | Entropy-based field-boundary suggestions as field-map fragments, scored |
| POST | `/api/assist/crc` | T-091 | CRC/BCH parameter search over a capture's frames → `crc`/`bch` parameter objects, scored |
| GET | `/ws/open/inspector` | T-089 | `?pipeline=<id>[&output=<id>]` or `?capture=<id>[&from_frame][&field_map=<recipe_id>@<version>:<map_id>]`: an inspector stream (§14.8). TCP: `open/inspector?…` |
| GET | `/ws/open/stage` | T-088 | `?pipeline=<id>&node=<node>[&port=<port>][&view=raw\|spectrum]`: a stage stream (§14.4). TCP: `open/stage?…` |
| GET | `/ws/inspector/{pipeline_id}/{output_id}` | T-088 | The always-on inspector stream of a running pipeline output (`/ws/{stream_id}` form) |

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
- **`codes[]`**: `{kind (crc | bch), width, generator (with the x^w term), poly (without), start_bit, tail_bits, bit_order (air | byte-reflected), classes, init, xorout, init_resolved, class_constants[], cross_length, method (exhaustive | catalogue), cyclic? {n, k, covered_bits, name?}, known_as?, reveng? {name, params, byte_order, field_endianness}, validated, tested, differences, evidence_bits, score, reasons[], fragment}`. Frames are counted once however often (and in whichever class) they repeat; `differences` = validated distinct frames − constants, not counting differences that repeat with a period ≤ 16 bits (constant/alternating input), and `evidence_bits = width × differences − log2(hypotheses)` (≥ 16 to be listed). **`score`** (absolute) = validated share × `(1 − e^(−evidence_bits/16))` × a generator confidence: every divisor of a fitting generator fits too, and with few differences their GCD can carry a chance factor (3 Mode-S frames: CRC-24 × a small factor fits). The largest fitting generator gets `exp(−6·Σ_k I_k·2^(−k·differences))` (`I_k` irreducible polynomials of degree `k`: 3 frames ≈ 0.1, 4 ≈ 0.4, 8 ≈ 0.95); a divisor of it stays listed as an alternative, scored with `2^(d−1−d·differences)` (`d` the degree gap), while that is ≥ 0.05, with a reason naming the pair. Values are bit-serial over the air-order bits; `reveng` maps a byte-aligned CRC onto the RevEng model when one reproduces the frames. `classes > 1` means frames grouped by index mod `classes` each have their own constant (`class_constants`, e.g. RDS offset words).
- **`parity[]`**: `{scope: frame {start_bit, tail_bits} | character {char_bits, phase}, parity, validated, tested, score, fragment}`.
- **`suggestions[]`** (fields): `{name, kind (constant | counter | length | high-entropy | mixed | check), bit_offset, bit_len (null = varies), from_end, value_hex?, length_scale?, length_add?, mean_entropy, mean_constancy, score, reasons[]}`; `field_map` is the matching draft `FieldMap` (unit bits) and `field_map_errors` its static validation errors. `per_bit[]` is `{coverage, ones, entropy, transition}` per bit position for plotting. Structural scores are chance-corrected with `1 − e^(−bits/8)` of significance bits: `constant` × P(random bits this constant over this many frames, any start), `counter` × P(random values stepping by one on that many pairs), `length` × P(random values matching the lengths), `check` = the code score; `high-entropy` is the mean entropy and `mixed` 0.5 (descriptive, not claims). With `align`, frames are aligned inside the work cap (`frames_aligned` counts the frames kept). Frames longer than 4096 bits are not classified (empty answer, reason in `work.skipped`).
- **Errors:** `400 invalid` (bad bits, unknown key, both or neither of `bits`/`frames`), `415` (not JSON), `405` for other methods, `503 busy` while another assist call is computing.

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
