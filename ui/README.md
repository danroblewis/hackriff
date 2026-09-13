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

T-044/T-045 add live-view interaction and a checked frequency axis:

- **Frequency axis.** Every connection's stream header re-derives the geometry (a new replay pass,
  a restarted server or a retuned source never keeps an old centre/span). Spectrum bin `i` of `N`
  is at `center_hz + (i − ⌊N/2⌋)·bandwidth_hz/N` (`StreamHeader::spectrum_bin_hz`, hk-dsp order);
  `src/axis.ts` holds the pure mapping (bin ↔ Hz ↔ pixel, zoom, ticks). The bar above the
  waterfall shows centre, span and bin width.
- **Readout.** Hover, touch or pen press shows frequency (the centre of the bin under the pointer),
  level and, over the waterfall, the row time. Pointer maths uses the canvas bounding rect.
- **Drag to select.** Drag across the spectrum or waterfall to add a region; drag vertically in the
  waterfall to bound its time too. Selections are client-side objects
  (`src/selections.ts`: `{id, name, f_lo, f_hi, t_lo?, t_hi?, created}`, Hz and Unix seconds),
  several at once, listed under **Selections** with rename, delete, **Zoom** (live view) and
  **History** (region over time). Demod, record and inspect are disabled stubs until T-052, which
  persists selections server-side. **Reset zoom** returns to the whole band.
- **Click to inspect.** A click or tap looks up the nearest emitter in `/api/inventory` around that
  frequency and shows its status, extent, family, identity (withheld as the server withholds it),
  first/last seen, count and tags. **Listen** is a disabled placeholder for T-043.

T-051 adds the **control panel** (beside the live view on wide screens, below it on phones), over
the T-050 control API; the SDR++/SDRangel/SigDigger survey and endpoint mapping are in
[CONTROLS.md](CONTROLS.md).

- **Driven by `GET /api/control/state`** (polled every 2 s and after every action): device
  capabilities, tuning, `run.content_class` (e.g. "restricted-paging: metadata only"), segment,
  display settings and recording. A segment change (a legal-class re-plumb) is announced.
- **Radio:** centre entry with units (`101.3M`, `433.92 MHz`, `+25k`; a bare `101.3` is MHz), step
  and ◀ ▶ shifts (fixed steps snap to their grid; ½ span / span walk a band), span from
  `device.sample_rates_hz`, named gain sliders from `device.gain_stages` (LNA/VGA and an amp switch
  on a HackRF, the device's own names otherwise), bias tee behind a DC-on-antenna warning. One
  device request at a time with a spinner (a re-plumb can take 30 s). On a replay the whole group
  is disabled with the `not_live` reason.
- **View:** drag the frequency scale to pan, scroll or pinch to zoom (client-side, `axis.ts`
  `panView`/`zoomAt`). Panning past the band edge offers a retune; it never retunes on its own.
- **Display:** pause/resume (only the spectrum stream pauses), FFT size, averaging and waterfall
  speed (`/api/control/display`); colour scale auto or manual min/max and peak hold (client-side).
  The waterfall and persistence shaders max-pool the texels under each pixel's own footprint
  (`axis.poolWindow`), fixing the T-045 half-window bias.
- **Record IQ:** start (label, max seconds) and stop, with the state and any refusal reason
  (recording is refused under content-forbidding classes).
- **Markers & bookmarks** (`/api/bookmarks`): a marker at the last clicked frequency, a bookmark
  from a selection (**Bookmark** in the selections table), list, jump (zoom inside the band; retune
  outside it when live), delete; drawn as lines over the live view.

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
just test-ui         # build + tsc --noEmit + npm test; `just test` runs it, and skips it when node is absent
```

`npm test` bundles `test/*.test.ts` with esbuild and runs them with `node --test` (no extra
dependencies): axis mapping, selection model and inspect lookup. `test/spectrum_axis.golden.json`
holds the header geometry and peak bins that `tests/e2e/tests/spectrum_axis.rs` observes through
the real pipeline (a tone at centre + 500 kHz and the FM fixture's 101.3 MHz station);
`HK_UPDATE_GOLDEN=1 cargo test -p hk-e2e --test spectrum_axis` rewrites it.

## Run

```sh
just ui-build
# Live HackRF One (receive only; the `hackrf` feature links the system libhackrf):
cargo run -p hk-cli --features hackrf --bin hk -- serve --hackrf \
  --center-hz 100.8e6 --rate 2.4e6 --lna 32 --vga 30 --amp
# Or an explicit recording:
cargo run -p hk-cli --bin hk -- serve \
  --replay fixtures/hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta --loop
# prints: open http://127.0.0.1:8787/#token=<64 hex>
```

`hk serve` runs the whole pipeline (spectrum, detection, tracking, inventory, history) over its
source, so `/api/inventory`, `/api/history`, `/api/floor` and `/api/status` show only what that
run detected. There is no demo data: a fresh data directory starts with an empty inventory.

`hk serve` options:
- `--hackrf[=SERIAL]` (the default source) with `--center-hz`, `--rate`, `--lna`, `--vga`,
  `--amp`; or `--replay FILE [--loop]`
- `--data-dir DIR` (default: a fresh temp directory)
- `--calibration FILE` (T-021 CalibrationState JSON) for calibrated floors
- `--bind ADDR` (default `127.0.0.1:8787`)
- `--ui-dist DIR` (default `ui/dist`)
- `--rows-per-s R` (default 25)

The live source's content class comes from its window (band-derived restricted classes); a UI
retune to a window of another class is refused (`hk_api::LiveControl`).

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
- **Listen (T-043)** opens `ws(s)://<host>/ws/open/listen?emitter=<id>` (or `f_lo=&f_hi=` in Hz)
  `&token=`, relative to the page so it works through the tunnel (`docs/stream-contract.md` §12).
  - **First text message:** the audio header (`ri16_le` mono at 48 kS/s, plus the `audio` profile:
    mode chosen by the server, estimated parameters, squelch and AGC), or a refusal
    `{"type":"refused","status","reason",...}` followed by a close with code 4000 + status.
  - **Binary records:** type 1 is 20 ms of PCM, type 2 is a drop marker, and type 3 is status
    JSON (level, SNR, squelch, AGC gain, latency).
  - **Playback:** Web Audio through an AudioWorklet (`dist/audio-worklet.js`, same origin), or a
    ScriptProcessor on insecure origins. Both use the jitter buffer in `src/jitter.ts`: 150 ms
    prebuffer, oldest audio dropped above 600 ms, underruns counted.
  - The AudioContext is created in the click handler, which unlocks audio on mobile.
  - Closing the player closes the socket, which detaches the server's chain.

## Security notes

- **Token.** Every `/api/*` and `/ws/*` request needs the server's token, which is generated at
  start (256-bit) or taken from `HK_TOKEN`.
  - The page reads it from the URL fragment (`#token=`), which is never sent to the server. It
    keeps the token in `sessionStorage` for the tab and strips it from the address bar.
  - Fetches send `Authorization: Bearer`. WebSockets send `?token=`, because browsers cannot set
    WebSocket headers. Control calls (POST/PUT/DELETE) never put the token in the URL
    (`controls/client.ts` refuses to build one); the server refuses `?token=` for them.
  - Through the cloudflared tunnel (or from another device), paste the token from
    `~/.config/hackriff/api-token` into the prompt; **Forget token** clears it from the tab.
    All URLs are origin-relative, so the page uses https/wss through the tunnel.
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
- **Control (T-050).** Mutating endpoints are audited server-side and receive-only; the panel has
  no transmit control.
