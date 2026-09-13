# Spike S3 — Web waterfall frame rate (Mac render prototype)

**Date:** 2026-09-13 · **Spike:** docs/09 §S3 · **Gates:** ADR-0002 · **Status:** Mac prototype measured; Jetson + phone **PENDING**

*(Written by the spike agent; saved to disk by the coordinator because the agent harness blocked the subagent from writing the .md file.)*

## Machine / browser
- Mac Studio, Apple M3 Ultra (28-core CPU / 60-core GPU), 256 GB, macOS 15.5 (24F74); display 5120×1440 @ 60 Hz.
- Chromium: Playwright's cached "Google Chrome for Testing" 151.0.7922.34 (chromium-1234), driven by `playwright-core` 1.63.0 via `executablePath`.
- Launch flags: `--use-angle=metal --enable-gpu --ignore-gpu-blocklist --disable-background-timer-throttling --disable-renderer-backgrounding --disable-backgrounding-occluded-windows`.
- WebGL renderer in **both headless and headed**: `ANGLE (Apple, ANGLE Metal Renderer: Apple M3 Ultra)` — real GPU, **not SwiftShader**. MAX_TEXTURE_SIZE 16384; EXT_color_buffer_float available.
- Headless rAF is not vsync-locked (~76 fps, jittery); headed rAF is locked to 60 Hz. **Headed numbers are the realistic ones.**

## What was built
| Part | Path | Notes |
|---|---|---|
| Frame server (Rust, axum 0.8 + tokio) | `server/` | Standalone Cargo project (empty `[workspace]`). Serves `client/dist` and `/ws?bins=&fps=&dtype=u8\|f32`. Synthetic dBFS spectrum: exponential noise floor with band-edge tilt, 4 drifting CW tones, a wobbling FM-like hump, an 8 s chirp, random 20–300 ms bursts. Generator → bounded queue (depth 4) → socket; queue full ⇒ frame **dropped, generator never blocks** (ADR-0004 policy), so the client sees seq gaps. |
| Client (TypeScript, esbuild, no framework, no WASM) | `client/src/main.ts` (~300 lines) | WebGL2. |
| Bench | `bench/bench.mjs`, `bench/run-matrix.sh`, `bench/table.mjs` | Starts the server, launches Chromium, warms up 3 s, measures N s, writes `results/*.json`. |

**Wire format.** First WebSocket message = JSON text header `{"type":"hk.spectrum.header","schema":"s3-spike/1","bins","center_hz","span_hz","fps","dtype","units":"dBFS","u8_min_db":-130,"u8_max_db":-10,"frame_header_bytes":24}`. Every later message is binary: 24-byte little-endian header `seq:u32 dtype:u8 ver:u8 rsv:u16 bins:u32 rsv:u32 t_unix_ms:f64`, then `bins` × f32 dBFS or u8. The 24-byte header keeps the f32 payload 4-aligned, so the client reads it as a zero-copy Float32Array.

**Render path (per rAF).**
1. Each received row is written with `texSubImage2D` into one row of an R8 texture ring (bins × 512).
2. **Waterfall:** a full-screen triangle; the fragment shader maps a head/UV-offset uniform to the ring row (no scroll copy), **max-decimates** `ceil(bins/px)` texels per pixel (13 at 16384 bins on 1280 px), colour map in the shader.
3. **DPX persistence on the GPU (the heavy option):** two R16F render targets (bins × 256 levels) ping-ponged, one full-texture pass **per received row**: `H' = β·H + hit`, the hit spanning adjacent bins' levels (connected trace), β = exp(−1/(τ·fps)), τ = 0.5 s. At 16384 × 256 × 60 rows/s that is ~252 M texel updates/s. Display pass: max over bins per pixel → log intensity → colour map.
4. **Spectrum line:** `LINE_STRIP` with no vertex buffer; the vertex shader reads the newest row via `texelFetch` indexed by `gl_VertexID`.
5. `persist=cpu` for comparison: the same histogram in a JS Float32Array, uploaded whole as R32F each rAF.
6. `dtype=f32`: a JS loop converts f32 dBFS → u8 per frame.

**Metrics:** rAF fps; rAF interval p50/p99 (frame time); rows drawn/s; seq gaps (server/transport drops); client drops (> 16 rows pending per rAF); latency from `t_unix_ms` → rAF (same host) and WebSocket receive → rAF; JS work inside rAF. `finish=1` adds `gl.finish()`, which turned out **not** to block in Chrome; `finish=2` adds a 1-pixel `readPixels`, forcing a GPU sync, so work ≈ CPU + GPU frame cost. Browser CPU % = `ps` cputime delta of the whole Chromium process tree (browser + GPU + renderer helpers + the idle node bench script) / wall time; 100 % = one core. Server CPU % measured the same way.

## Results (60 s per run; 1280×800 at DPR 1 unless noted)
| mode | bins | in fps | wire | persist | notes | rAF fps | frame p50/p99 ms | rows/s | seq gaps | client drops | ts→rAF p50/p99 ms | rAF work p50/p99 ms | browser CPU % | server CPU % |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| headless | 4096 | 30 | u8 | gpu | | 76.14 | 11.6 / 27.3 | 30.01 | 0 | 0 | 5.8 / 25.4 | 0 / 0.2 | 24.2 | 0.6 |
| headless | 4096 | 60 | u8 | gpu | | 77.09 | 11.4 / 27.3 | 60.02 | 0 | 0 | 5.7 / 24.7 | 0 / 0.2 | 24.8 | 0.9 |
| headless | 16384 | 30 | u8 | gpu | | 78.28 | 11.2 / 27.3 | 30.01 | 0 | 0 | 6.1 / 26.0 | 0 / 0.2 | 21.3 | 0.9 |
| headless | 16384 | 60 | u8 | gpu | | 77.18 | 11.4 / 27.3 | 60.02 | 0 | 0 | 6.2 / 25.1 | 0 / 0.2 | 23.5 | 1.6 |
| headed | 4096 | 30 | u8 | gpu | | 59.98 | 16.7 / 18.6 | 30.00 | 0 | 0 | 6.4 / 17.5 | 0.1 / 0.3 | 24.7 | 0.6 |
| headed | 4096 | 60 | u8 | gpu | | 59.98 | 16.7 / 18.7 | 60.01 | 0 | 0 | 10.0 / 17.4 | 0.1 / 0.3 | 22.7 | 1.0 |
| headed | 16384 | 30 | u8 | gpu | | 59.98 | 16.7 / 18.6 | 30.00 | 0 | 0 | 7.9 / 17.2 | 0.1 / 0.3 | 27.8 | 1.2 |
| headed | 16384 | 60 | u8 | gpu | | 59.98 | 16.7 / 18.6 | 60.03 | 0 | 0 | 7.8 / 17.4 | 0.1 / 0.3 | 34.9 | 2.4 |
| headed | 16384 | 60 | f32 | gpu | | 59.98 | 16.7 / 18.7 | 60.03 | 0 | 0 | 9.4 / 17.6 | 0 / 0.3 | 22.7 | 1.5 |
| headed | 16384 | 60 | u8 | gpu | gl.finish (non-blocking) | 59.98 | 16.7 / 18.7 | 60.01 | 0 | 0 | 6.3 / 18.0 | 0.1 / 0.3 | 34.0 | 2.4 |
| headed | 16384 | 60 | u8 | gpu | **readPixels sync** | 59.98 | 16.7 / 18.7 | 60.03 | 0 | 0 | 10.3 / 17.3 | **2.3 / 3.4** | 30.6 | 2.0 |
| headed | 16384 | **240** | u8 | gpu | **readPixels sync** | 59.98 | 16.7 / 18.7 | 240.06 | 0 | 0 | 8.2 / 17.3 | **1.8 / 3.8** | 26.0 | 4.5 |
| headed | 4096 | 30 | u8 | cpu | | 59.98 | 16.7 / 18.5 | 30.00 | 0 | 0 | 9.0 / 17.4 | 0.7 / 3.0 | 12.8 | 0.2 |
| headed | 16384 | 30 | u8 | cpu | | 59.98 | 16.7 / 18.7 | 30.02 | 0 | 0 | 6.7 / 17.5 | 3.5 / 4.3 | 20.3 | 0.4 |
| headed | 16384 | 60 | f32 | gpu | CDP CPU throttle ×6 | 59.98 | 16.7 / 18.6 | 60.03 | 0 | 0 | 10.3 / 17.6 | 0 / 0.5 | 96.4 | 0.8 |
| headed | 16384 | 60 | u8 | gpu | 2560×1440 | 59.98 | 16.7 / 18.6 | 60.01 | 0 | 0 | 7.4 / 17.2 | 0.1 / 0.2 | 35.4 | 2.5 |

## Findings
- Every headed configuration holds the 60 Hz display rate (frame-time p99 18.7 ms) with zero seq gaps and zero client drops, up to 16384 bins, 60 rows/s, full GPU DPX persistence, and a 2560×1440 canvas.
- **GPU headroom:** with a forced sync, the whole frame (upload + persistence passes + waterfall + DPX + line) costs 2.3 ms p50 / 3.4 ms p99 at 16384 bins × 60 rows/s. At 4× that load (240 rows/s, ~1 G texel updates/s) it is still 1.8 / 3.8 ms at 60 fps — about **5–8× headroom** against a 16.7 ms frame on this GPU.
- **CPU:** the client's JS work per rAF is ≤ 0.3 ms p99 once persistence is on the GPU. Under a ×6 CPU throttle (a crude slow-CPU proxy that does not slow the GPU) it still holds 60 fps with f32 input. Browser-tree CPU is 20–35 % of one core, mostly Chromium's compositor and GPU process, not our code.
- **CPU-side persistence** costs 3.5 ms per frame at 16384 bins (JS decay + full R32F upload): workable here but ~15× the GPU path's JS cost. Keep persistence on the GPU, or in the core as C39 suggests.
- **Latency** from frame generation to draw: 6–10 ms p50, ~17.5 ms p99 (bounded by one vsync), well inside C39's 100 ms target.
- **Bandwidth:** u8 4096×30 ≈ 0.12 MB/s; u8 16384×60 ≈ 1.0 MB/s; f32 16384×60 ≈ 3.9 MB/s. For remote clients send u8 or display-decimated bins.
- **Caveat:** headless mode is not a valid fps measurement (no vsync; p99 27 ms from scheduling jitter), though it did use the Metal GPU.

## Provisional verdict (Mac only)
**PASS for the render prototype on the Mac.** A thin-TS WebGL2 client with GPU DPX persistence holds 60 fps at up to 16384 bins with large GPU and CPU headroom. Nothing here argues for the ADR-0002 native-shell fallback.

This does **not** settle S3. The docs/09 criteria (≥ 30 fps in the Jetson kiosk browser, ≥ 15 fps on a phone over Wi-Fi) are unmeasured. The Jetson Orin Nano GPU is far weaker than an M3 Ultra (unverified estimate: an order of magnitude or more). The 5–8× headroom here suggests 4096 bins will be comfortable and 16384 × 60 may be marginal. If needed: reduce levels to 128, keep persistence at display rate, or move persistence into the core.

## Pending hardware checks — user test procedure
Build (needs Rust and Node ≥ 20):
```sh
cd spikes/s3-web-waterfall/server && cargo build --release
cd ../client && npm ci && npm run build
cd ../server && ./target/release/s3-frame-server --bind 0.0.0.0:8080 --dist ../client/dist
```
Find the LAN IP (`ipconfig getifaddr en0` on macOS, `hostname -I` on the Jetson) and allow TCP 8080 through any firewall.

**A. Jetson on-device (kiosk)** — run the server on the Jetson, then:
```sh
chromium-browser --kiosk --ignore-gpu-blocklist --enable-gpu-rasterization "http://127.0.0.1:8080/?bins=4096&fps=30&dtype=u8&persist=gpu"
```
1. The overlay's first line must name the NVIDIA GPU. If it shows SwiftShader or llvmpipe, fix acceleration via `chrome://gpu` first; the result is invalid otherwise.
2. Run each URL ≥ 60 s and note `rAF fps`, `seq-gaps`, `client-drop`, `latency p50`: `bins=4096&fps=30`, `bins=4096&fps=60`, `bins=16384&fps=30`, `bins=16384&fps=60`, `bins=16384&fps=60&dtype=f32` (all `persist=gpu`).
3. Log `tegrastats` during each run and note `sudo nvpmodel -q`.
4. Optional automated run: `cd client && CHROMIUM=$(which chromium) node ../bench/bench.mjs --mode headed --bins 4096 --fps 30`. Snap Chromium may refuse Playwright; reading the overlay by hand is enough.
5. **PASS:** ≥ 30 fps with seq-gaps and client-drop ≈ 0, at least at `bins=4096&fps=30&persist=gpu`.

**B. Phone over Wi-Fi** — server bound to 0.0.0.0 on the Mac or Jetson:
1. Open `http://<host-LAN-IP>:8080/?bins=4096&fps=30&dtype=u8&persist=gpu` in Safari or Chrome, screen on, page in the foreground.
2. Run ≥ 60 s each at `bins=4096&fps=30`, `bins=4096&fps=15`, `bins=16384&fps=30`. Record phone model, browser, the renderer line, and whether `(no float RT)` or `tex N` below the bin count appears (`tex N` = the client max-decimates to the phone's texture limit, expected).
3. Cross-host `ts→rAF` latency includes clock offset — indicative only. `seq-gaps` show Wi-Fi drops.
4. **PASS:** ≥ 15 fps with seq-gaps ≈ 0 at `bins=4096&fps=30&persist=gpu`.

Send back the overlay lines (a screenshot is fine) for each URL.

## Notes for the architecture
- **Browsers can't open raw TCP/UDS.** The ADR-0004 contract needs a **WebSocket bridge** for web clients. It maps one-to-one: the JSON header becomes the first text message; each length-prefixed record becomes one binary message (WebSocket already frames, so the length prefix is dropped). The drop-on-full bounded queue here is the ADR-0004 slow-consumer policy.
- C39 suggests persistence computed in the core and sent as an image. This spike accumulated on the client GPU (the heavier client case). Core-side persistence would lighten weak clients but costs remote bandwidth.
- No WASM was needed; it only matters for CPU-side persistence or f32 conversion on weak CPUs.

## Reproduce (Mac)
```sh
cd spikes/s3-web-waterfall/server && cargo build --release
cd ../client && npm ci && npm run build && npx tsc --noEmit
# uses ~/Library/Caches/ms-playwright/chromium-1234/chrome-mac-arm64/Google Chrome for Testing.app, or set CHROMIUM=/path/to/chrome
cd .. && sh bench/run-matrix.sh 60    # 16 runs, ~65 s each; headed runs open a window
node bench/table.mjs
# interactive:
cd server && ./target/release/s3-frame-server --bind 127.0.0.1:8080 --dist ../client/dist
open "http://127.0.0.1:8080/?bins=16384&fps=60&dtype=u8&persist=gpu"
```
URL params: `bins`, `fps`, `dtype` (u8|f32), `persist` (gpu|cpu|off), `levels` (256), `rows` (512), `tau` (0.5), `finish` (1 = gl.finish, 2 = readPixels sync), `ws`.

## Dependency licences
All permissive; **no GPL/LGPL**. Add to the ADR-0010 ledger if promoted beyond the spike.

**npm** (dev-only; `dist/main.js` bundles only our code):
| Package | Version | Licence |
|---|---|---|
| esbuild (+ @esbuild/darwin-arm64) | 0.28.2 | MIT |
| typescript (+ @typescript/typescript-darwin-arm64) | 7.0.2 | Apache-2.0 |
| playwright-core | 1.63.0 | Apache-2.0 |

The measurement browser (Chrome for Testing 151, BSD-3-Clause + third-party licences) is not a product dependency.

**Crates** (77 resolved, aarch64-apple-darwin):
| Licence | Crates |
|---|---|
| MIT | axum 0.8.9, axum-core, bytes, data-encoding, generic-array, http-body, http-body-util, http-range-header, hyper 1.11.1, hyper-util, mime_guess, mio, slab, tokio 1.53.1, tokio-macros, tokio-tungstenite 0.29.0, tokio-util, tower, tower-http 0.6.11, tower-layer, tower-service, tracing, tracing-core, zmij |
| MIT OR Apache-2.0 | atomic-waker, base64, bitflags, block-buffer, cfg-if, cpufeatures, crypto-common, digest, form_urlencoded, futures-channel/core/sink/task/util, getrandom, http, httparse, httpdate, itoa, libc, log, mime, once_cell, percent-encoding, pin-project-lite, ppv-lite86, proc-macro2, quote, rand, rand_chacha, rand_core, serde, serde_core, serde_derive, serde_json, serde_path_to_error, serde_urlencoded, sha1, smallvec, socket2, syn, thiserror, thiserror-impl, tungstenite, typenum, unicase, version_check |
| MIT AND BSD-3-Clause | matchit |
| Unlicense OR MIT | memchr |
| Apache-2.0 OR BSL-1.0 | ryu |
| Apache-2.0 | sync_wrapper |
| (MIT OR Apache-2.0) AND Unicode-3.0 | unicode-ident |
| BSD-2-Clause OR Apache-2.0 OR MIT | zerocopy |
