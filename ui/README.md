# ui/ — web client

TypeScript web client (ADR-0002), served by `hk serve` (later `hackriffd`) through `hk-api`. No
framework and no WASM yet; the TypeScript stays thin (about 600 lines) and all analysis stays in
Rust. T-022a covers:

- **Live view.** WebGL2 waterfall, spectrum line and GPU DPX persistence, ported from spike S3
  (`spikes/s3-web-waterfall/`), fed by one hk-stream spectrum stream over the WebSocket bridge.
  The header shows fps, rows, dropped and gated counts, the stream class, and a **GATED** badge when
  the class forbids content. Rows after a dropped or gated run are marked on the waterfall's left
  edge (magenta for dropped, amber for gated). Hover or touch shows frequency and dB.
- **Region over time.** Pick f lo/f hi (MHz) and a UTC time range, then **Load**. `/api/history`
  (T-017) is drawn as a heatmap of max, mean, low percentile or occupancy, with unobserved cells
  in grey ("not observed" is not "quiet"). `/api/floor` (T-021) is drawn as a floor-vs-time line
  with its uncertainty band. **Live span** fills the live stream's band and time.

T-022 adds:

- **Signal inventory.** A table of emitters from `/api/inventory` (the T-018 inventory query).
  - **Columns:** status badge (known / unexpected here / unknown; hover shows the prior's
    reason), MHz, bandwidth, family, identity, first/last seen (UTC), count, tags.
  - **Server-side filters:** region (f lo/f hi), time window, status, tag. **History region**
    copies the region-over-time band and window into the filters.
  - **Search** filters the loaded rows by id, identity, scheme, family or tag.
  - Column headers sort the loaded rows. **Load more** follows the server cursor, 200 rows a
    page.
  - **Clicking a row** shades its band on the waterfall (when it is inside the live span) and
    loads it, padded, in region over time.
  - On phones, bandwidth, first seen and tags are hidden, and the table scrolls sideways.
  - **Identity gating is server-side.** An identity from a restricted, own-key or unclassified
    source arrives as `withheld: true` with its scheme and no value, and shows as
    `<scheme>: withheld`. The page never receives the value.

## Build

Needs Node ≥ 20. Dev dependencies are esbuild (MIT) and TypeScript (Apache-2.0); `dist/main.js`
bundles only this directory's code.

```sh
just ui-build        # npm ci + esbuild → ui/dist/ (gitignored)
just test-ui         # build + tsc --noEmit; `just test` runs it, and skips it when node is absent
```

## Run

```sh
just ui-build
cargo run -p hk-cli --bin hk -- serve \
  --replay fixtures/hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta \
  --history-dir /tmp/hk-history --loop
# prints: open http://127.0.0.1:8787/#token=<64 hex>
```

`hk serve` options:
- `--bind ADDR` (default `127.0.0.1:8787`)
- `--history-dir DIR` enables `/api/history` and `/api/floor`
- `--inventory-db FILE` serves an hk-model SQLite inventory at `/api/inventory` (read-only)

**Inventory demo.** The replay does not yet run detection → tracking → inventory; that
composition is T-027. Until then, seed a demo database and serve it next to the FM fixture:

```sh
cargo run -p hk-api --example seed_inventory -- /tmp/hk-inventory.db   # 7 emitters around 11:10 UTC
cargo run -p hk-cli --bin hk -- serve \
  --replay fixtures/hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta \
  --history-dir /tmp/hk-history --inventory-db /tmp/hk-inventory.db --loop
```

Any database written by the T-018 inventory adapter (`hk_detect::track::inventory`) also works.
The seed includes a withheld pager capcode, a withheld own-key sensor and a withheld unclassified
talkgroup, so the gating is visible. The hk-api tests use the same seed.
- `--ui-dist DIR` (default `ui/dist`)
- `--fft N` (default 4096)
- `--rows-per-s R` (default 25)
- `--loop`

`HK_TOKEN` sets a fixed token of at least 16 characters.

Replay notes:
- Each `--loop` pass is a new stream, and the page reconnects on its own.
- Only the first pass is written to history, because later passes repeat the same timestamps.
- The FM fixture is 5 s long and was captured on 2026-09-13 at about 11:10 UTC. **Live span**
  fills that time range.

## Wire format

See `docs/stream-contract.md` §10.
- **First message:** text, the stream header JSON.
- **Each later message:** one record, binary for spectrum. It starts with the 32-byte
  little-endian record header: type, flags, length, seq, `t` in ns, and sample index.
- **Spectrum rows** from `hk serve` are `rf32_le`: `fft_size` values of PSD in dBFS/Hz, in
  ascending frequency over `center_hz ± bandwidth_hz/2`.
- **Record type 2** is a drop marker. With the `GATED` flag, the rows were withheld by the egress
  gate. Without it, they were dropped by the queue.

## Security notes

- **Token.** Every `/api/*` and `/ws/*` request needs the server's token, which is generated at
  start (256-bit) or taken from `HK_TOKEN`.
  - The page reads it from the URL fragment (`#token=`), which is never sent to the server. It
    keeps the token in `sessionStorage` for the tab and strips it from the address bar.
  - Fetches send `Authorization: Bearer`. WebSockets send `?token=`, because browsers cannot set
    WebSocket headers.
  - Comparison is constant time. Static files (the UI code) need no token and contain no data.
- **Bind address.** The default is loopback. `--bind 0.0.0.0:8787` exposes the API to everyone on
  the LAN or Wi-Fi. There is no TLS in M0, so the token is the only protection and travels in
  cleartext. Only do this on a trusted network, e.g. to view from a phone.
- **Own-key content is never served.** Every browser consumer subscribes as `Locality::Remote`,
  even from localhost, so the stream contract refuses `own-key-decrypted` streams (HTTP 403).
  Every other egress gate applies unchanged: class clamping, and the gated spectrum rate and size
  limits.
- **Backpressure.** A slow browser only fills its own bounded queue. It gets drop markers and is
  disconnected after 5 s; the producer never waits. Streams also cap their consumers (HTTP 503
  beyond the cap).
- **Read-only.** M0 endpoints are all `GET`; there is no control surface yet.
