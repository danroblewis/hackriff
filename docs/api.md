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
- **Errors.** Every non-2xx JSON body is `{"error": "<message>"}`; every route reached through the control dispatcher (`control.rs`/`selections.rs`/`outputs.rs`/`inventory.rs` — everything except the five read-only endpoints in the first table below and `/ws/*`) additionally carries a stable machine `"code"`: `{"error", "code"}`. Error messages never echo raw request values. Common codes: `invalid` (400, malformed/out-of-range field), `not_found` (404), `unauthorized` (401), `forbidden`/cross-origin (403), `not_live` (409, device settings on a replayed recording), `conflict` (409, a re-plumb or another operation is in progress), `refused` (409, legal/content-class gate said no), `finished` (409, the run has ended), `timeout` (504), `out_of_range` (400, a device value outside its capabilities), `unsupported` (501, the device lacks the capability, e.g. no bias tee), `not_implemented` (501, a route is defined but its engine isn't built yet, e.g. `POST /api/analyze` until MAUTO), `busy`/`quota` (503/507, output-recording admission), `unavailable` (503, the server has no audit log / bookmark store / output recorder / etc. for this feature).
- **Audit.** Every **mutating** request to `/api/control/*`, `/api/bookmarks*`, `/api/selections*`, `/api/outputs*` or `/api/inventory/{id}*` is written to the run's audit log (`<data dir>/control-audit.jsonl`, mode `0600`) once authenticated: time, token id (never the token), peer, method, path, action name, request body, old/new values, status, result. Unauthenticated mutating attempts are logged too, coalesced per client to bound disk use. **`GET` requests are never audited**, on any route. Without an audit log every mutating endpoint answers `503 unavailable`. See `crates/hk-api/src/control.rs` module docs for the exact schema.
- **Bounded resources.** At most `ServerConfig::max_connections` (default 64) connection threads at once (WebSocket consumers included); request heads ≤ 16 KiB, bodies ≤ 64 KiB, both within `request_timeout` (default 10 s); `/api/history`/`/api/floor`/`/api/inventory` cap result size (below).
- **Receive only.** No route reaches a transmit path; `transmit.available` is always `false` (C37 stays gated at the type level, not just by convention — there is no transmit operation to call).

## Read-only query routes

| Method | Path | Auth | Query | Response | Errors |
|---|---|---|---|---|---|
| GET | `/api/streams` | token | – | Discovery document (T-060, below) | 401 |
| GET | `/api/history` | token | `f_lo`, `f_hi` (Hz), `t0`, `t1` (Unix s), `max_cells`? (default 100 000, max 500 000), `format`? (`json` default, `csv`, `png`), `stat`? (csv/png: `max`, `mean`, `p_low`, `p_high`, `floor`), `source`? (16 hex digits or `unknown`), `site`? (`unassigned`, `mobile`, a site id or `unknown`) | T-017 region-over-time grid (below); T-116 hackrf_sweep CSV or PNG waterfall; T-133 source/site filter | 400 invalid region/cells/format/stat/source/site, 404 no history store on this server |
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
    { "name": "symbols", "ws_path": "/ws/open/symbols", "tcp_target": "open/symbols", "kind": "symbols", "...": "…" }
  ],
  "tcp": { "addr": "127.0.0.1:8788",
           "handshake": "<tcp_target>?token=<token>[&param=value...]\\n",
           "refusal": "one frame {\"type\":\"refused\",\"status\",\"code\",\"reason\"} instead of the header" }
}
```

`tcp` is `null` when no TCP stream server runs. See [Streams](#streams-websocket-tcp-and-on-demand-openers) below and `docs/stream-contract.md` §10/§12/§13 for what each named stream/opener actually carries.

`dc_excluded_hz` (T-167, ADR-0013 §4.9 gap 10) is the half-width, Hz, of the DC/LO-leakage notch centred on `center_hz` that the producer's own detector excludes from analysis (the spectrum stream's `hk-pipeline` producer sets it from `hk_detect::DcRule::default().tolerance_hz`, the same value `GET /api/observations` `records[].window.dc_excluded` already reflects). It is additive on both `/api/streams` and the stream header itself (below) and `null` when a producer applies no DC mask to that stream — never a guess.

### `GET /api/history` — region-over-time grid (T-017, AWARE-042)

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
  "filter": null
}
```

T-116 additions (all additive):

- `coverage` is each cell's observed fraction of its duration; `coverage_summary.gaps` lists maximal time runs in which **no** cell of the grid was observed. A gap is never reported as quiet.
- `floor_db` is the noise-floor estimate: `p_low_db` corrected for the low-percentile bias of averaged-periodogram noise (Gamma model, shape `provenance.cell_shape`). T-141: a tile whose frames had different shapes (a scheduler's short-step rows) is corrected with the bias of the Gamma **mixture** of its level-0 values, weighted by the values folded per shape over the tile (`provenance.cell_shapes`). `null` when the frames carried no noise shape, when a mixed tile recorded no per-shape counts (tile format < 4, more than 32 shapes: `provenance.other_shape_values` > 0), or when its shapes' frames covered different numbers of cells (`values`/`frames` not equal across `cell_shapes`: a cell's percentile pools only its own frames, so the tile's weights would not be the cell's); `p_low_db` stays the raw percentile.
- T-141 (additive, tile format 4): `provenance.cell_shapes` lists `{shape, values, frames}` (level-0 cell values and frames folded per cell shape; a shape within 5 % of a listed one counts under the first seen; at most 32) and `provenance.other_shape_values` counts values whose shape is unrecorded.
- `provenance` records gain table, filter/antenna port, spur-mask version and cell shape (first value plus a `*_mixed` flag) and every front-end change as a `steps` entry (time, what changed, state before and after; at most 32, the rest counted in `steps_dropped`). Cells are not split at a step — use the steps to explain level changes as provenance, not events. `scheme` is the pyramid scheme/version id and `tile_format` the tile format written.

T-133 additions (all additive; tile format 3, formats 1 and 2 still read):

- **Origins.** `provenance.origins` lists the frames behind the whole result per origin, at most 8 origins for the whole result (not per tile); frames of further origins are summed in `other_origin_frames`. An origin is `source`, the 16-hex-digit key of the source (`hk_store::history::source_key` of the run's `device_id`), plus `site`, the site at the frame's sample time (`unassigned`, `mobile` or a site id; ADR-0012 §3.5). `null` means **unknown**: tiles written before format 3, frames without a site, or frames over a tile's 8-origin cap.
- **Filter.** `source` and `site` restrict the grid to the frames of that source and/or site (both given: both must match). `unknown` selects frames of unknown source or site. Each tile also keeps at most 8 origins and counts the frames of further origins as unknown, so `site=unknown` (or `source=unknown`) can include such overflow frames, and a tile that overflowed is never a whole match for a specific source or site. Tiles are not split by origin, so a cell into which frames of other origins were folded reads `null` (**unobserved**, not quiet). The exception is a cell of a coarse tile that mixed origins: it is kept when the one finer tile it rolls up matches whole, so a site change costs about one finer tile's duration of coverage. Unknown-origin (old) history matches only an unfiltered request or `unknown`.
- **Filter disclosure.** `filter` is `null` without a filter, else `{source, site, tiles_matched, tiles_mixed, tiles_other, cells_excluded, cells_from_children}`; `source`/`site` are `null` when that field is not filtered. `provenance` then merges only the tiles whose data the grid returns. CSV and PNG exports honour the filter.

**Exports.** `format=csv` returns `text/csv; charset=utf-8` in the `hackrf_sweep` line format `date, time, hz_low, hz_high, bin_width, num_samples, dB…` (UTC; dB per `bin_width`-wide bin, i.e. `stat` dB/Hz + 10·log10(bin_width); `num_samples` = largest frame count in the line; 256 cells per line; unobserved cells `nan`, fully unobserved lines omitted). `stat` defaults to `mean`. `format=png` returns an `image/png` waterfall (one pixel per cell, frequency left→right, earliest time at the top, 8-bit palette; grey = not observed; colour range 2nd percentile to maximum of `stat`, default `max`). Errors are JSON as for every endpoint.

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

**Mixed cell shapes (T-141).** A run whose history rows average different segment counts (the scheduler's short steps) folds tiles of several Gamma cell shapes. Each tile decides from its own persisted record: a uniform tile's cells use their shape's bias; a mixed tile's cells use the bias of the Gamma mixture of the tile's level-0 values (weights = values folded per shape, `provenance.cell_shapes`; the percentile's CDF point is solved on `Σ wᵢ·P(nᵢ, nᵢ·x)`), so `bias_db` then lies between the components' biases. The mixture is used only when every shape's frames folded equally many values (`values`/`frames` equal): otherwise frames of one shape covered fewer or other cells (short hops beside full-span dwells), some cells' own mix differs from the tile's, and the tile gives no floor rather than a wrong one. Weights stay per tile, not per time column: columns whose shape composition differs from the tile's are corrected with the tile's mix (the step median absorbs this). A mixed tile written before tile format 4 (no per-shape counts) contributes no cells, as before; `shape` stays the product's first shape. No field changed.

### `GET /api/inventory` — signal inventory (T-018, T-078, AWARE-053/AWARE-042)

Query parameters (all optional, combined with AND): `f_lo`&`f_hi` (Hz, given together), `t0`&`t1` (Unix s, given together), `state` (comma-separated `candidate`/`confirmed`/`deleted`; **default: candidate and confirmed — deleted entries are listed only when `deleted` is explicitly asked for**), `status` (comma-separated `known`/`unexpected-here`/`unknown`), `tag`, `scheme` (identity scheme), `family`, `relations` (`shown` default / `all`; T-219, below), `cursor` (row offset, ≤ 1 000 000), `limit` (default 100, max 500).

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
      "known_status": "known",
      "status": { "status": "known", "author": "prior", "t_s": 1789300810.0,
                  "reason": "on FM broadcast allocation", "prior_ref": "band-plan/us-fm@1", "reason_withheld": false },
      "tags": [], "tags_withheld": false, "family": "wfm-broadcast",
      "classification": { "family": "wfm-broadcast", "confidence": 0.9, "open_set_score": 0.1,
                           "model_version": "…", "t_s": 1789300820.0,
                           "taxonomy": null, "stage": "chain", "arb_rank": 3, "coarse": null,
                           "class": null, "top": null, "entropy_norm": null, "flags": null },
      "latest_classification": null,
      "classifications": 3,
      "identity_scheme": "rds-pi", "identity_class": "unrestricted", "withheld": false,
      "identity_value": "A1B2",
      "snr_db": 21.4, "peak_dbfs": -18.25,
      "relation": null
    }
  ],
  "next_cursor": null, "limit": 100, "total": 214, "identity_access": "standard"
}
```

`identity_value` is present only when the row's identity is in clear (`withheld: false`); on a withheld row a status/lifecycle reason from an author who may have seen the identity is itself withheld (`reason_withheld: true`, `reason: null`). Never included: decode content, fingerprints, links. No frequency lookup ever runs before detection — the inventory is populated purely from blind measurement (vision step 4); the band-plan/licence database only supplies `explanations` and `status`, ranked, never a starting point.

**`snr_db` / `peak_dbfs` (T-158).** The emitter's latest measurement: the peak SNR (`snr_peak_db`) and absolute peak level (`peak_level_dbfs`) of the newest (highest start time) detection linked to it, read directly off the stored `Detection` — no separate computation. "Linked" follows the same track a row's sighting created: a detection counted through one of the emitter's currently-linked tracks (the common case — sightings are almost always offered as tracks), or linked to the emitter directly. Both fields are `null` together when the emitter has no linked detection yet (e.g. an identity-only sighting from a decode, or a brand-new candidate before its track is offered). They are never derived from `recurrence` or any other summary field.

**`relation` and `relations=` (T-219, C40).** One physical signal can produce several overlapping inventory rows, and a receiver can manufacture a row out of thin air. `relation` says why a row **defers to another**, or is `null` (the normal case): `{"kind", "artifact", "source_id", "author", "actor", "t_s", "reason", "score", "detail"}`, where `kind` is `suppressed-by` (the row overlaps a **Confirmed** entry's band by at least 60 % of **both** the narrower and the wider band, with nothing to tell the two apart — so a narrow emission sitting inside a wide one is never hidden by it), `duplicate-of` (the weaker of two overlapping candidates, ranked by a provisional SNR × duty × trust proxy — `score` — until decode evidence in bits exists), or `artifact-of` (a receiver artifact, with `artifact` ∈ `image` / `harmonic` / `intermod` and `detail` carrying the arithmetic: `n`, `a`, `b`, the tuning centre used, `predicted_hz`, `error_hz`, `tolerance_hz`, `suppression_db`). `reason` is backend-rendered and never names an identity. Rows with a standing relation are **hidden by default** and listed with `relations=all`; the default is `relations=shown`, and an unknown value is `400 invalid`.

**Nothing is ever deleted or overwritten by this.** A deferring row keeps its id, count, detections, tracks, links and history, is still reachable at `GET /api/inventory/{id}`, and its claim is append-only and reversible — later evidence revokes it and the row is listed again. A relationship is ranked evidence with its reasoning disclosed, never truth (the exploration-first rule). **The guard:** band overlap alone only makes two rows compete; any distinguishing evidence blocks a claim, in order — two different decoded identities, measured bandwidths further apart than the clustering ratio (checked on the measurement, so it holds for rows with no fingerprint), a fingerprint distance beyond tolerance, then −3 dB extents separated by more than the measurement uncertainty. Two genuinely distinct adjacent stations therefore stay two entries. An `artifact-of` claim needs more than arithmetic: the measured bandwidth must match the width the mechanism implies (an image preserves it, an `n`th harmonic scales it by `n`), the level must be 10–80 dB below the source, the row must have been seen only while the source was on air, and the detection must itself carry the matching suspect flag (`image_candidate`, `spur_candidate` or `suspect_imd`) — `detail.corroborating_flag` names it. Rules and thresholds: `hk_model::relate`; ADR-0015 §11.4.

**`classification` / `latest_classification` (T-211, ADR-0016 §2).** `classification` is the classification that sets `family`: the lowest **arbitration rank** (`arb_rank` 0 user > 1 decoder > 2 lock-verified > 3 classifier > 4 track shape), latest among equals, so `family`, `classification.family` and the `family` filter always agree. `latest_classification` is the most recently appended row when that is a different row (e.g. a later rank-3 `unknown` under a rank-2 lock-verified label), else `null`. Both have the same shape, or are `null` when the emitter has no classification:
- `family`, `confidence`, `open_set_score`, `model_version`, `t_s`: as before.
- `stage` (`feature-tree` / `verifier` / `dl` / `decoder` / `user` / `chain` / `track-shape`) and `arb_rank` (0–4). They are always set: a row written before M3 derives them. A `model_version` starting `decoder:` gives `decoder`/1, a track input gives `track-shape`/4, and anything else gives `chain`/3.
- `taxonomy` (e.g. `"hk-mod@1"`), `coarse` (`analog` / `digital` / `noise-like` / `unknown`), `class` (`{label, p, stage}` within the family, or `null` below its gate), `top` (≤ 5 posterior labels `{label, p}`, highest first, `unknown` included), `entropy_norm` (0–1) and `flags` (`prior-tiebreak`, `prior-mismatch`, `below-gate`, `suspect-input`, `dl-shadow-disagrees`). All are `null` on a row written before M3 or by a pre-M3 writer.

The full classification (likelihood, prior, provenance, reasons) is not on the row; it is served per emitter by the planned `/api/inventory/{id}/classification` (T-199). `family` values on M3 rows are `hk-mod@1` families (`analog`, `fsk`, `psk-qam`, …, or `unknown`); pre-M3 rows keep their labels (`wfm`, `2fsk`, decoder and service ids).

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

### `GET /api/inventory/{id}/decode` — latest decode fields (T-159, ADR-0013 API GAP 3)

The emitter's most recently decoded fields: the focus panel's "Decoded summary" and per-signal output panels (RDS PS/RT for FM, decoded records for digital recipes; docs/14 "Added scope from docs/15 §7"). `{id}` resolves like `/api/inventory/{id}` (a merged id resolves to its live survivor). No parsing happens in the UI — every field here is as the decoder or recipe committed it.

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

One row per `(decoder, frame_model)` pair the emitter's decoded identity has produced, newest first: a plugin decoder (`readsb`, or the built-in `hk-rds`) commits one frame model per row, while a recipe's several `messages` outputs share one `decoder` (`recipe:<id>`, e.g. `rds.recipe.json`'s `group-info`/`station`/`radiotext` outputs) but each names its own `frame_model`, so RDS's PI/group metadata, PS and RadioText each get their own row. `recipe_id` is the id after `recipe:` when `decoder` has that prefix, else `null`. `fields` merges the row's metadata (frame type, addresses, counts — always stored) and content (payload/text, only when the caller's identity access reveals it; content gating is off by default, T-143) into one object, exactly as the decoder/recipe committed them. `crc.valid` is whether the frame's check passed; `source_session` is the producing `Demodulation`'s id, else the replayed `Recording`'s id, else `null` for a live decode with neither. An emitter with no decoded identity, or one content gating withholds, answers `{"decodes": []}` — the same lookup never confirms a withheld identity by naming its decodes (T-036). `404 not_found` for an unknown id.

**Evidence rule (T-185/T-210).** A CRC-invalid frame never reaches a row: the `fields` block's `skip_invalid` drops it before parsing, so `crc.valid` is `true` for every row today. T-210 (in progress) adds bounded RDS block error correction with consensus-gated PI/PS/RT commits; **TODO(T-210):** once corrected-group provenance lands on `Decode`, add `crc.corrected` here without changing what `crc.valid` means.

**User band (T-191).** Every row (list and one entry) carries `user_band`: `null`, or `{"f_lo", "f_hi", "set_at", "actor", "reason", "reason_withheld"}` — edges in Hz, `set_at` in Unix s, `actor` the token fingerprint, `reason` the user's note or `null` (withheld, with `reason_withheld: true`, on a withheld-identity row like any user-authored reason). It is a user's adjustment of the band edges (e.g. dragging a confirmed signal's box) stored **beside** the measured band: `f_center_hz`/`bandwidth_hz`/`f_lo_hz`/`f_hi_hz` stay what blind detection measured and are never overwritten. Rules for `PUT`: `f_lo` and `f_hi` finite numbers with `0 < f_lo < f_hi`; width `f_hi − f_lo` ≤ 40 MHz (`hk_model::USER_BAND_MAX_WIDTH_HZ`); and the band must overlap the measured `[f_lo_hz, f_hi_hz]` or lie within 1 MHz of it (`USER_BAND_MAX_GAP_HZ`; both limits inclusive) — an adjusted edge, not a different signal. Set and clear are both audited as `inventory_band` with `old`/`new` = `{"id", "user_band"}` and the token fingerprint as actor. The override survives restart; when two entries merge (same emission, T-082) the survivor keeps an override if either had one, the latest `set_at` winning (a tie keeps the survivor's). The pipeline never uses it for detection, tracking or entity resolution; consumers that tune to an entry (Listen, Decode, recipes' `{emitter_id}` target) still use the measured band today and may prefer `user_band` later.

### `GET /api/analysis/strongest` — strongest signal in a band (T-079)

Backend replacement for client-side peak-picking over a locally held spectrum row (see [UI decision logic moved server-side](#ui-decision-logic-moved-server-side-t-079) below): the strongest observed signal (max-hold, dB/Hz) in `[f_lo, f_hi)` over the last `window_s` seconds, read from the same spectrum-history pyramid as `/api/history`. The window ends at the stream time the history has reached (the end of its newest frame), not the wall clock, so a replay or a time-compressed scene (T-125) is queried on its own clock; before any frame it ends at the wall clock.

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

**Baselines and attention (T-119, T-132, ADR-0012 §3)** are reported under `"attention"`: counters `folds`, `novel_folds`, `change_points`, `publishes`, `baseline_writes`, `errors`, plus the baseline memory bound. Loaded baselines are capped at 256 MiB by default (`HK_BASELINE_MEMORY_MB`, `0` = unbounded).

| Field | Type | Meaning |
|---|---|---|
| `attention.memory_bytes` | number | Gauge: approximate heap bytes of the loaded baselines |
| `attention.unloaded_engines` | number | Baseline keys saved and unloaded by the cap (reloaded on their next fold) |
| `attention.refused_folds` | number | Folds whose learning the cap refused; their novelty is still scored against what is loaded |
| `attention.gain_overflow_folds` | number | Folds under a gain state beyond a subject's kept gain slots (4 per level class); scored, not learned |

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

## Analyze / synthesize decoder (stub, T-190)

`POST /api/analyze` is the MUI "Analyze / synthesize decoder" action (docs/14, `docs/15-decoder-synthesis.md` §7): point the guided search at a signal — a selection, an inventory emitter, or an ad-hoc band, optionally over a past window of the IQ ring rather than live — and it streams back the best decoding pipeline and its evidence, attaching the result to the emitter. That engine (MAUTO, `docs/15-decoder-synthesis.md` §8: guided search over demod/framing/FEC structure and parameters, evidence-metric scoring, template priors) is not scheduled yet. Until it lands, this route only **validates and resolves its target** and answers `501`; the future response shape (the streamed best-pipeline-plus-evidence contract) is defined by ADR-0015 (PROVISIONAL) and lands when MAUTO is scheduled, not previewed here.

| Method | Path | Body | Response |
|---|---|---|---|
| POST | `/api/analyze` | `{"selection_id"} \| {"emitter_id"} \| {"band": {"f_lo", "f_hi", "t_lo"?, "t_hi"?}}` (exactly one) | `501 {"error": "analyze is not implemented yet", "code": "not_implemented"}` once the target validates |

Give exactly one of `selection_id`/`emitter_id`/`band`, else `400 invalid`; an unknown field anywhere in the body (or in `band`) is `400 invalid`; `band.f_lo >= band.f_hi` is `400 invalid`. `404 not_found` for an unknown selection or emitter, including a malformed id — `{id}` may name an entity since merged into another and resolves to the live emitter, exactly like `GET /api/inventory/{id}`. `503 unavailable` when this server has no selection store or no inventory. Authenticated and audited (`analyze`) like every other mutating control route (same bearer-token, header-only and cross-origin rules); no engine runs and nothing is recorded.

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
- **`Segment`**: `{id, run, t0, t1, t0_ns, t1_ns, samples, global_index, center_hz, sample_rate_hz, bandwidth_hz, lna_db, vga_db, amp_on, device_id, antenna_port, overload, content_class, dropped_before}`.
  - `id` increases for the life of the ring.
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

## Classification taxonomy (T-218, ADR-0016 §1–§2)

| Method | Path | Body / query | Response |
|---|---|---|---|
| GET | `/api/taxonomy` | – | `{"current", "unknown", "taxonomies": [...], "thresholds": {...}, "coarse": [...]}` |

The modulation taxonomy and the decision thresholds **as data**, so the thin client never keeps its own copy of the family tree, the label spellings or the SNR gates (a stale copy would tell a different story from the one the backend decided). Code: `hk_model::classify::{taxonomy, thresholds}`, served by `crates/hk-api/src/taxonomy.rs`.

- **`current`** is the taxonomy new classifications are written under (`"hk-mod@1"`); **`unknown`** is the open-set label (`"unknown"`), which is an outcome at every level and never a leaf of the tree.
- **`taxonomies`**: every *released* version, oldest first — a stored row keeps the version it was written under, so a reader maps its labels with the matching entry. Each is `{"ref", "name", "version", "families": [{"family", "coarse", "classes": [...]}], "legacy": [{"label", "family"}]}`. `coarse` is `analog` / `digital` / `noise-like`. `legacy` maps pre-taxonomy spellings (e.g. `fsk2` → `fsk`); labels that are already a family or class name are not repeated there. Service labels (`adsb`, `fm-broadcast`, decoder ids) are **not** modulation labels and appear nowhere here.
- **`thresholds`**: `{"version": "thresholds@1", "max_confidence", "lambda0_min", "families": [{"family", "snr_gate_db", "class_gate_db", "min_confidence", "open_set_max"}]}`. `snr_gate_db` is `null` for a family with no SNR gate (`noise-like` is a shape test). Below its gate a family contributes no likelihood mass — its share moves to `unknown` with reason `low_snr`, because "not measured" is not "ruled out". `max_confidence` (0.999) is the cap that keeps any call from being reported as certain; `lambda0_min` (0.1) is the smallest uniform weight a C17 prior may carry, which is what stops a band-plan prior from driving a family to zero.
- **Reference data, not measurement.** No emitter, detection or identity is reachable through this route, and it pre-populates nothing: what was actually *found* comes from `/api/inventory`, always from blind detection first. Errors: `405` (other methods), `401` (token).

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
- **Audit.** Mutating routes are audited (`recipe_save`, `recipe_delete`, `pipeline_start`, `pipeline_edit`, `pipeline_save`, `pipeline_stop`, `pipeline_channels`, `pipeline_channels_refresh`) with ids and revisions, not whole documents. `POST /api/recipes/validate` saves nothing: it needs the token in the header like every POST but is not audited.

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
| PUT | `/api/pipelines/{id}/channels` | `{channels_hz: [Hz, ...]}` | Follow-hops pipelines (T-093/T-107). 200 `{id, channels: [{index, center_hz, bandwidth_hz}], added: [channel], removed: [index], applied_at_sample}` (`applied_at_sample` null when nothing changed). A running channel within a quarter channel bandwidth of a requested one keeps its instance and state; new channels start at a chunk boundary without a gap on the others; missing ones stop. Channel indices are never reused. Refusals: 400 `invalid` (not an array of positive frequencies), 404, 409 `ended`, 409 `outside_window` (a channel is not inside the tuned window, also when a retune lands before the change applies; nothing changes), 422 `not_follow_hops`, 422 `no_channels`, 422 `too_many_channels` (above the recipe's `max_channels`), 503 `busy` (each added channel claims one chain of the budget; nothing changes), 504 `timeout` |
| POST | `/api/pipelines/{id}/channels/refresh` | – | Re-resolves the pipeline's channel source (`list_hz`, the hop-set emitter's measured `hop_set_hz`, or the blind detections in its band) and applies it like `PUT …/channels`; same answer and refusals |
| DELETE | `/api/pipelines/{id}` | – | `{"stopped": pipeline}`: the pipeline stops, its streams finish and `/api/streams` no longer lists them |

**Pipeline** JSON: `id` (`p<n>`), `recipe_id`, `recipe_version`, `edit_rev` (0 = as started), `state` (`running` \| `ended`), `end_reason` (`stopped`, `source-ended`, `segment-ended`, `retune: …`, `rate-change: …`, `error: node <id>: …`), `target`, `channel: {center_hz, bandwidth_hz, sample_rate_hz}`, `content_class`, `emitter_id`, `started` (Unix s), `nodes: [{id, block, outputs}]` (topological order), `outputs: [{id, kind (inspector \| stage \| messages), stream_id}]` (a `messages` output is listed only while it offers a stream, see below), `status` (the latest status tick: flat `<node>.<metric>` keys, ADR-0011 §1.3), `stats: {samples, chunks, frames, gaps, discontinuities, skipped_samples, edits, status_ticks, decodes, decodes_dropped}` (`decodes`: Decode rows the `messages` outputs stored; `decodes_dropped`: frames dropped because a writer queue was full), `warnings`.

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

**Streams of a pipeline** (stream contract §14; discovery lists them under `/api/streams`):

| Stream | How to open | Carries |
|---|---|---|
| `inspector/<pipeline>/<output>` | `GET /ws/inspector/<pipeline>/<output>` (the `/ws/{stream_id}` route), TCP `inspector/<pipeline>/<output>?token=…`, or the opener `GET /ws/open/inspector?pipeline=<id>[&output=<id>]` / TCP `open/inspector?…` (first inspector output by default) | messages stream, `message_schema: hackriff.inspector/1`: one frame record per frame, one `status` record per ~250 ms tick (every node batched), one `edit` record per applied edit |
| `stage/<pipeline>/<output>` | `/ws/stage/<pipeline>/<output>` or TCP (a recipe's declared `stage` outputs) | §14.4 binary records, one per processed chunk |
| `decodes/<pipeline>/<output>` | `/ws/decodes/<pipeline>/<output>` or TCP (a recipe's `messages` outputs, T-111) | messages stream, `message_schema: hackriff.decode/1` (as a plugin's `decodes/<plugin>`): one record per stored Decode row, republished through the stream gate. Under a class that forbids content it is offered only when the recipe's `output_policy` declares an allowlist; the rows are stored either way |
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
- Refusals: 400 `bad-request` (bad `capture`, `from_frame` or `field_map` syntax), 404 `not-found` (capture, recipe version or map), 422 `unreadable`/`invalid`, 503 `busy` (4 capture replays are already streaming on this server; no queueing, retry when one ends, like assist's `busy`).

Errors are `{"error", "code"}`: `400 invalid` (bad id or query, messages never echo values), `404 not_found`, `405` (with `Allow`), `409 conflict`, `422 unreadable` (not a readable inspector stream), `500 unreadable` (store I/O), `503 unavailable` (no capture store on this server).

## Observation log (T-115; ADR-0012 §1)

Where and when the radio actually observed, and why: one `DwellRecord` per non-sweep scheduler step and one `SweepRecord` per discovery pass or 60 s, whichever ends first (hops reference a `SweepGeometry`). Schemas: `hk_model::attention::observation`. Records come from the scheduler of a scheduler-driven run (`hackriffd`, `hk run --schedule`); a live run without the scheduler (`hk serve`) logs its interactive tuning as `interactive`-tier dwell records (one per steady tune, closed on retune and split every 60 s; they add coverage and observed seconds, never activity-independent visits); a recording replayed without the scheduler logs nothing. Frequencies in Hz; times in Unix seconds on the sample clock the captured blocks carry (a replay reports the recording's time).

- **Observed extent.** `window.usable` is the analysed spectrum frame's extent (the span history tiles fold, so log and tile coverage describe the same cells), clipped to the sampled band; `window.dc_excluded` is the ±15 kHz DC notch. A frequency range counts as observed only while it lies **entirely** inside `usable` minus the notch.
- **Observed interval.** `observed` starts when the analysed data carries the step's tuning (retune settle) and ends when the next step starts, clipped to `planned`; `preempted` marks a step cut before its planned end. Sweep `visits[]` give `start_ms`/`observed_ms` after the record's `span.t0`.
- **Freshness.** Queries read the segment files plus the writer's unflushed buffer. Records are dropped (and counted in `log.dropped`) only when the writer queue is full; the pipeline never waits for the log.
- **Storage.** Hourly CRC-line segments `<data>/observations/YYYY/MM/DD/HH.log`, flushed at most once a minute (or at 256 KiB), fsynced at hour seal, kept 30 days / 512 MiB by sample time.

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/api/observations?f_lo&f_hi&t0&t1[&tier][&cursor][&limit]` | token | Records overlapping the box, in log order |
| GET | `/api/observations/coverage?f_lo&f_hi&t0&t1[&channel_hz][&tau_s][&min_gap_s]` | token | `ObservationTotals`, per-channel totals, gaps and POI rows |

`GET /api/observations` → `200`:

```json
{
  "f_lo": 100000000.0, "f_hi": 102000000.0, "t0": 1789300800.0, "t1": 1789300860.0,
  "records": [
    { "record": "sweep", "schema": 1, "survey_id": "…", "plan_version": 1, "site": { "kind": "unassigned" },
      "geometry": 1234567890123, "span": { "start": "…", "end": "…" },
      "visits": [ { "hop": 0, "start_ms": 0, "observed_ms": 50 } ],
      "preempted_hops": 0, "dropped_samples": 0, "overload_hops": 0 },
    { "record": "dwell", "schema": 1, "seq": 42, "plan_version": 1, "site": { "kind": "unassigned" },
      "reason": { "code": "poi-dwell", "poi": 3 }, "tier": "bandit",
      "window": { "center_hz": 100500000.0, "sample_rate_hz": 2400000.0,
                  "usable": { "lo_hz": 99300000.0, "hi_hz": 101698828.1 },
                  "dc_excluded": { "lo_hz": 100485000.0, "hi_hz": 100515000.0 }, "rbw_hz": 3515.6 },
      "rf_path": 0, "planned": { "start": "…", "end": "…" }, "observed": { "start": "…", "end": "…" },
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
- Times inside records (`span`, `planned`, `observed`, and `totals.span`) are integer Unix nanoseconds (hk-model `Timestamp`); the top-level `t0`/`t1` and `gaps` are seconds.

`GET /api/observations/coverage` → `200`:

```json
{
  "f_lo": 100990000.0, "f_hi": 101010000.0, "t0": 1789300800.0, "t1": 1789300860.0,
  "totals": { "freq": { "lo_hz": 100990000.0, "hi_hz": 101010000.0 }, "span": { "start": "…", "end": "…" },
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
           "interval": {"start": "…", "end": "…"}, "fco": 0.12, "fco_all_visits": 0.31, "fco_suspect_upper": 0.13, "fbo": 0.08,
           "n_revisits": 64, "n_occupied": 8, "n_suspect": 1, "n_revisits_all": 120, "observed_s": 61.5, "revisit_max_s": 41.0, "revisit_mean_s": 14.1,
           "timing": "unknown", "threshold": {"method": {"method": "dynamic", "idle_fraction": 0.8}, "guard_db": 5.0, "rbw_correction": true},
           "threshold_db": -121.4, "guard_clamped": true, "rbw_hz": 6250.0, "obw_hz": 15000.0, "unit": "…",
           "confidence": {"lo": 0.06, "hi": 0.22, "level": "p95", "n_eff": 58.3, "independence_assumed": false},
           "revisit_biased": false, "fco_window": {"start": "…", "end": "…"}, "subject_extent": {"f_lo_hz": 433475000.0, "f_hi_hz": 433500000.0}}],
 "coverage": {"rows": 1, "rows_with_fco": 1, "observed_s": 61.5, "unobserved_is_not_quiet": true}}
```

- **Rows** follow ADR-0012 §2.1 (`OccupancyStat`, `hk_model::attention::occupancy`) plus `subject_extent` (the subject's frequency extent in Hz). `sro` is present on band rows only. Absent optional fields are omitted.
- **`interval`:** `15m` (default) and `1h` read the persisted series (closed every 15 min of stream time; `1h` rows at hour boundaries); `span` computes one row per subject over exactly `[t0, t1]` from the history, the final channel plan and the detections (band ≤ 20 MHz, span ≤ 7 days, band × span ≤ 120 MHz·h, at most 2 M visit samples; the history is read in bounded chunks). Series closes evaluate every band observed in the interval (each dwell window and sweep hop of the observation log), so a retune inside an interval keeps both bands.
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
| GET | `/api/baselines` | token | `site`? (default current; 409 when mobile/unassigned) | `{"site", "slot", "baselines": [{"site", "cal": {"kind": "uncalibrated"\|"calibrated", "id"?}, "scheme", "cell_factor", "subjects", "mature_subjects", "finest_resolution"\|null, "last_visit", "change_points": [{"subject", "f_lo", "f_hi", "t", "statistic": "level"\|"occupancy", "direction": 1\|-1, "cusum"}]}]}` |
| GET | `/api/baselines/slots` | token | `f_lo`, `f_hi` (required), `site`?, `slot`? (0–167 hour-of-week; default now at the site), `resolution`? (`hour-of-week`, `hour-of-day`, `day-part`, `all-hours`; default the finest mature) | `{"site", "slot", "subjects": [{"subject": {"kind": "cell", "index"}\|{"kind": "channel", "key"}, "f_lo", "f_hi", "cal", "maturity": {"state": "mature", "resolution"}\|{"state": "immature", "observed_s"}, "mixed", "gain_states", "reference": pool, "adaptive": pool, "change_point"\|null, "refrozen_at"\|null}], "truncated"}` (at most 2 000 subjects) |
| POST | `/api/baselines/refreeze` | token | `{"site"?, "f_lo"?, "f_hi"?}` (both edges or neither) | `{"site", "refrozen": n}`: adaptive copy → frozen reference, change points cleared |
| GET | `/api/candidates` | token | `f_lo`?, `f_hi`?, `limit`? (1–1000, default 100) | `{"version", "t", "site": site_key, "weights", "candidates": [Candidate], "truncated"}`: the latest published `CandidateSet`, `score` descending. Populated on every run (T-131): with the bandit off (the default) the control thread publishes the set read-only (nothing schedules from it); with `extra.bandit` it is the bandit's provider |
| GET | `/api/attention/weights` | token | – | `{"weights": {"version", "snr", "novelty", "class_entropy", "decoder", "periodicity", "boring"}, "defaults": weights, "history": [{"version", "created", "author"}]}` (newest first; version 1 = defaults, never stored) |
| PUT | `/api/attention/weights` | token | all six weights, each in [0, 10], at least one positive term | `{"weights"}` with the next version; takes effect at the next scoring pass, never retroactively |

- **site** = `{"id", "name"|null, "lat_deg"|null, "lon_deg"|null, "radius_m", "utc_offset_min", "source": "config"|"user"|"gnss", "first_seen", "last_seen", "observed_s"}`; **site_key** = `{"kind": "site", "id"}`, `{"kind": "mobile"}` or `{"kind": "unassigned"}`. Only `site` keys build baselines; mobile and unassigned folds are kept as occupancy but never accrue (ADR-0012 §3.5). Occupancy rows carry the site assigned when their 15-min interval closes (T-131), so pinning the current site starts baselines, and each close steps the novelty alarms (`/api/anomalies`, the `anomalies` stream) on `hk run` and `hk serve` alike. The current assignment (site, `set_by`, `pinned`, last in-site time) is stored in the run database and restored when a service reopens it (T-136): a pinned site stays pinned with no `unassigned` gap, and a GNSS site keeps its no-fix hold on the sample clock.
- **pool** = `{"resolution", "n", "observed_s", "mean_db"|null, "std_db"|null, "fco"|null, "max_db"|null}`: the slot's pool at that resolution (levels from the most-visited gain state, occupancy over all).
- **Maturity** needs ≥ 24 h of observation in the pool; hour-of-week falls back to hour-of-day, day part, then all hours, and the resolution used is always disclosed. Immature pools give novelty 0.
- **Calibration** is part of the baseline key: a new calibration starts a new, immature baseline, so a calibration step is never novelty.
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

Inventory emitters and anomalies are not keyed by site or source. Schema: `hk_model::attention::report::SurveyReport` (wire structs reject unknown fields; every `Timestamp` is an integer of Unix **nanoseconds**, `FreqRange` is `{lo_hz, hi_hz}`, `TimeRange` is `{start, end}`).

- **`format=json`** (default): the document `{schema, generated_at, region, span, site, occupancy {bands, channels, truncated}, top_emitters[], change_vs_baseline {status, baseline?, resolution?, changes[]}, coverage, provenance_steps[], anomalies[], warnings[]}`. `change_vs_baseline.status` is `available` / `immature` / `no-baseline` (mobile or unassigned site, or no baselined subject) / `unavailable` (no baselines on this server); each `changes[]` entry `{subject, kind, baseline, observed, z}` combines the subject's 15-min rows, each compared against its own hour-of-week slot, with `kind` `level-above-baseline`, `busier-than-usual` (z > 0) or `quieter-than-usual` (z < 0). `generated_at` is the stream time the history has reached (never the wall clock).
- **Coverage is mandatory** (`coverage {observed_fraction, observed_s, gaps[{freq, time}], gaps_truncated, never_observed[], poi[{tau_s, p_poi}], statement}`): POI for τ = 5 ms, 100 ms, 1 s, 10 s; gaps are unobserved stretches longer than twice the measured mean revisit, coalesced across adjacent frequency cells, longest first (≤ 64); `statement` always says unobserved is not quiet. Coverage comes from the observation log (T-115) when it holds visits for the box, else from history-tile coverage (a replay without the scheduler logs nothing); `warnings` names the source and, when it holds nothing for the box before some time inside the span (e.g. the log started mid-span), says that time is shown as unobserved, not quiet. A server that cannot disclose coverage (no spectrum history) answers `404` instead of a report.
- **Occupancy.** One band row for the region and channel rows (FCO descending, ≤ 64, `truncated`) over **blind** channel extents: the inventory emitters' measured extents in the box, overlapping extents merged. `fco` is the unbiased figure from activity-independent visits only (ADR-0012 §2.5) and is never substituted: it is absent when the source cannot give it. Until the occupancy engine (T-118) lands, rows come from history-tile occupancy (floor + the pyramid margin), which mixes activity-driven dwells, so rows carry **no `fco`**: `fco_all_visits` = occupied grid rows / observed grid rows, `n_revisits_all` = observed grid rows (`n_revisits`/`n_occupied` 0), `revisit_biased: true`, `threshold.method: history-tile` with the pyramid `margin_db`, `fbo` = coverage-weighted tile occupancy, `timing: unknown`; channel rows sort by `fco`, then `fco_all_visits`; `warnings` says so.
- **Top emitters** (≤ 20, most sightings in the span first): `emitter_id`, measured `freq`, `first_seen`/`last_seen`, `sightings` in the span, `lifecycle` (`candidate`/`confirmed`), channel `fco` and `fco_all_visits` (each copied from its channel row, so no `fco` from the tile stand-in), `top_suggestion` (the top-ranked explanation's service label: a suggestion, never truth) and `new_in_span`.
- **Change vs baseline.** `status` is `unavailable` until baselines (T-119) land; `changes` is non-empty only when `available`. A warning states that no comparison is implied.
- **Provenance steps** (time order): `{t, kind, freq?, detail}` with `kind` `gain` (LNA/VGA/amp or gain table), `calibration`, `spur-mask`, `antenna-port`, `sample-drop`, … from the history tiles' provenance, e.g. `detail: "lna 32→24 dB"`. Overload share and mixed calibration appear in `warnings`.
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

## Route table completeness

`crates/hk-api/src/http.rs::ROUTES` is the single source of truth for what answers under `/api/` and `/ws/`; `crates/hk-cli/tests/api_contract.rs::every_route_in_the_route_table_is_documented` asserts every entry in it appears (method and path together) somewhere in this file, so this document cannot silently fall behind the server.

## Sources

- [ADR-0002](adr/0002-ui-web-vs-native.md) (the UI is the first client of the same API external programs use)
- [`docs/stream-contract.md`](stream-contract.md) — the versioned wire contract every stream and the TCP handshake follow
- [`docs/07` §2.11](07-data-model.md) (emitter lifecycle), [`docs/07` §2.20](07-data-model.md) (selections)
- `crates/hk-api/src/http.rs`, `control.rs`, `selections.rs`, `outputs.rs`, `inventory.rs`, `query.rs`, `ondemand.rs`, `bridge.rs`, `tcp.rs`, `auth.rs`
- `crates/hk-cli/src/serve.rs`, `pipeline.rs` (how `hk serve` composes the pipeline and this API)
