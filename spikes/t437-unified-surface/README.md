# T-437 — the unified full-spectrum surface (spike)

Throwaway. The deliverable is [`REPORT.md`](REPORT.md); this is how to re-run it.

```sh
# 1. a backend over the MOCK SDR (never 8789/8899/8900 - those are the live demo's)
cd /Users/daniellewis/hackriff
HK_TOKEN=t437spiketoken0123 ./target/debug/hk serve \
  --device mock:fixtures/hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta \
  --bind 127.0.0.1:18787 --data-dir /tmp/t437-data --ui-dist ui/dist --fft 4096 --rows-per-s 25

# 2. the spike server: static client + the STUBBED tile route
cd spikes/t437-unified-surface && node harness/serve.mjs --port 18790

# 3. open http://127.0.0.1:18790/ , or run the proofs headless:
node harness/prove.mjs     # -> results/prove.json + results/*.png
```

Give the backend ~90 s before running the proofs: the pyramid needs history to exist
before "grey means unobserved" can be told apart from "grey because nothing was captured
yet".

## Layout

| Path | What |
|---|---|
| `client/view.js` | The view scheme and the **de-welded** addressing. `level_f` and `level_t` are two numbers; nothing derives one from the other. |
| `client/tilecache.js` | The **shared** tile-texture LRU, keyed `(level_f, level_t, f_block, t_block)`. Counts uploads, hits, evictions, refetch-after-evict. |
| `client/render.js` | **One** WebGL2 context. `gl.viewport` + `gl.scissor` per pane. One colour ramp, one grey, one honesty-tier rule, in one shader. |
| `client/main.js` | The panes (live / history / time-nav / minimap), follow-mode, gestures, the retune *offer*. |
| `harness/serve.mjs` | Static server **and the stubbed tile route**. Written in Node deliberately so it cannot be mistaken for the real `/api/tile` (T-438 owns that). |
| `harness/cdp.mjs` | Dependency-free Chrome DevTools Protocol driver (node 24's built-in `WebSocket`). Real WebGL2 on ANGLE/Metal, headless. |
| `harness/prove.mjs` | The proof run. Every number in `REPORT.md` comes from `results/prove.json`. |

## What is a stub, and what is real

- **Real:** the backend (`hk serve`), the mock SDR device, the pyramid, `/api/history`,
  `/api/coverage`, `/api/navigation`, `/api/control/rate`, `/api/control/center`, the
  WebGL2 context, the textures, the measurements.
- **Stubbed:** the tile route. `harness/serve.mjs` maps `(level_f, level_t, f_block,
  t_block)` onto a `/api/history` + `/api/coverage` pair and packs an RGB8 tile. That is
  the *addressing* proved end to end; the *route* is T-438's.
- **Synthetic (and labelled as such):** `?stress=1` / `setStress(true)` generates tiles in
  the client so the renderer and the LRU can be costed with a realistic working set. Its
  tiles are always tagged `survey-overview` and never claim to be measurements.
