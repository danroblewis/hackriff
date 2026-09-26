# ui/ — web client

TypeScript web client (ADR-0002), served by `hk serve` (later `hackriffd`) through `hk-api`. No
framework and no WASM: vanilla TypeScript plus a small store (`src/app/store.ts`), per ADR-0013.
All signal analysis stays in Rust; the UI only renders, interacts and calls
[`docs/api.md`](../docs/api.md) (thin client, CLAUDE.md "Working conventions").

## The app (ADR-0013, T-149…T-156)

The **MUI** one-screen exploratory app is the only UI: `src/app/` builds to `dist/index.html`
(served at `/`, aliased at `/app.html`). The old stacked single-page layout (`src/main.ts`,
`src/index.html`, `src/style.css`) was retired by T-156.

Two modes via a top-bar toggle, an always-on Outputs dock and Capture timeline:

- **Explore:** left sidebar (Signal inventory — Candidates/Confirmed tabs — and Selections);
  centre (spectrum trace + WebGL2 waterfall, brackets, drag-to-select, hover readout); right focus
  panel (measurements and ranked, never-authoritative explanations).
- **Decode:** pipelines + recipe stages + a blocks palette; per-stage plots; the packet inspector
  (frame list → hex+ASCII → layer tree, linked selection both ways); stage parameters with blind
  "Use" suggestions and live quality tiles.
- **Outputs dock:** every live stream (audio, decoded records) this page opened, with
  meter/rate, mute, copy-address, stop.
- **Capture timeline:** always-on recording, a scrubbable band, "reviewing N ago" / LIVE.
- **Review drawer:** Alarms, Report, History, Scheduler, Device and Bookmarks — the M0b–M2 panels,
  rehomed rather than stacked at the bottom of the page. The Device tab's control survey (mapping
  to SDR++/SDRangel/SigDigger) is [CONTROLS.md](CONTROLS.md).

Component tree, state store, the frontend↔API map for every panel, and the responsive/theme rules
are [ADR-0013](../docs/adr/0013-ui-architecture.md) §2–§5; read that (and the capability cards it
names) before changing a panel. Each area owns `src/app/<area>/{index.ts,slice.ts,<area>.css}` and
mounts only its own `data-slot`s (`src/app/index.html`); cross-area calls go through the stub
contracts in `src/app/dock/api.ts` and `src/app/decode/status-feed.ts`.

**Reused pure helpers.** A handful of flat `src/*.ts` modules predate the area split and are kept
in place (not moved under `src/app/`) because several areas import from them: `axis.ts` (bin↔Hz↔
pixel mapping, zoom, ticks), `waterfall.ts` (the WebGL2 renderer), `selections.ts`
(`SelectionStore`, offline sync), `inventory.ts`/`alarms.ts`/`report.ts`/`scheduler.ts`/
`history.ts`/`frame-inspector.ts`/`inspect.ts` (query builders, formatters and wire types — their
old DOM-rendering classes were deleted by T-156), `outputs.ts`, `audio-frames.ts`, `jitter.ts`,
`audio-worklet.ts`, and `controls/{client,freq,model,bookmarks,gestures}.ts`.

## Build

Needs Node ≥ 20. Dev dependencies are esbuild (MIT) and TypeScript (Apache-2.0).

```sh
just ui-build        # npm ci + esbuild → ui/dist/ (gitignored)
just test-ui         # build + tsc --noEmit + npm test; `just test` runs it, and skips it when node is absent
```

`npm run build` produces `dist/index.html` (= `dist/app.html`, an identical alias), `dist/app.js`,
`dist/app.css` and `dist/audio-worklet.js`. Budget (ADR-0013 §1): `app.js` ≤ 150 KB minified/≤ 45 KB
gzip, `app.css` ≤ 40 KB — checked by eye on each build, not by an automated gate.

`npm test` bundles `test/*.test.ts` with esbuild and runs them with `node --test` (no DOM
available; layout rules are checked by reading the built CSS/HTML as text — see `app-shell.test.ts`
and the per-area `app-<panel>.test.ts` files' narrow-width checks). `test/spectrum_axis.golden.json`
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
source, so the inventory, history, floor and status routes show only what that run detected. There
is no demo data: a fresh data directory starts with an empty inventory.

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

### Client flags (`src/flags.ts`)

Flags are **query parameters on the page**, off by default, and each switches a *lane* — never an
honesty rule (a mark's claim is not a flag).

- **`?live-ring=1` — the live ring (T-1042 / LSR-1).** Every pane that is following the live edge
  paints its newest rows straight from `/ws/spectrum/live` (`src/surface/livering.ts`), and the tile
  lane skips the extent those rows cover: rows at the edge, the pyramid below them, which is the
  live-rendering invariant ("rows append as they are recorded; live is never gated on tile
  generation"). Off, the surface is drawn from tiles exactly as before.
  `HK_UI_LIVE_RING=1` is the same switch for a shell: `ui/e2e/harness.mjs`'s `appUrl` turns it into
  the query parameter, so `HK_UI_LIVE_RING=1 node e2e/run.mjs live-ring` and a page opened by hand
  agree. The page states what the lane did per pane in `.sf-stage[data-live-ring]` (rows painted,
  tile addresses excluded, one row's height in px) while the flag is on.

Replay notes:
- Each `--loop` pass is a new stream, and the page reconnects on its own.
- Only the first pass is written to history, because later passes repeat the same timestamps.
- The FM fixture is 5 s long and was captured on 2026-09-13 at about 11:10 UTC.

## Wire format

See `docs/stream-contract.md` §10.
- **First message:** text, the stream header JSON.
- **Each later message:** one record, binary for spectrum. It starts with the 32-byte
  little-endian record header: type, flags, length, seq, `t` in ns, and sample index.
- **A later *text* message whose `schema` is `hackriff.stream` is a new header** (T-417): a retune
  or a re-plumb offers a new publisher under the same `stream_id` and the bridge keeps this
  connection on it, so the socket does not die when the radio moves. Everything after that header
  describes the new window; the data genuinely gaps across the seam and nothing is drawn over it.
  `net.openStream` does this dispatch, so a panel only ever sees `onHeader` again.
- **Spectrum rows** from `hk serve` are `rf32_le`: `fft_size` values of PSD in dBFS/Hz, in
  ascending frequency over `center_hz ± bandwidth_hz/2`.
- **Record type 2** is a drop marker. With the `GATED` flag, the rows were withheld by the egress
  gate. Without it, they were dropped by the queue.
- **Listen** opens `ws(s)://<host>/ws/open/listen?emitter=<id>` (or `f_lo=&f_hi=` in Hz)
  `&token=`, relative to the page so it works through the tunnel (`docs/stream-contract.md` §12).
  - **First text message:** the audio header (`ri16_le` mono at 48 kS/s, plus the `audio` profile:
    mode chosen by the server, estimated parameters, squelch and AGC), or a refusal
    `{"type":"refused","status","reason",...}` followed by a close with code 4000 + status.
  - **Binary records:** type 1 is 20 ms of PCM, type 2 is a drop marker, and type 3 is status
    JSON (level, SNR, squelch, AGC gain, latency).
  - **Playback:** Web Audio through an AudioWorklet (`dist/audio-worklet.js`, same origin), or a
    ScriptProcessor on insecure origins. Both use the jitter buffer in `src/jitter.ts`: 150 ms
    prebuffer, oldest audio dropped above 600 ms, underruns counted.
  - The AudioContext is created in the click handler, which unlocks audio on mobile (the Outputs
    dock's `AudioSession`, `src/app/dock/audio-session.ts`, shares one `AudioContext` across
    every open Listen entry).
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
- **Control (T-050).** Mutating endpoints are audited server-side and receive-only; the UI has no
  transmit control.
