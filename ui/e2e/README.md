# `ui/e2e` — the browser tier (T-455)

Headless Chrome, driving `/surface.html` against a real `hk serve` over a recorded SigMF fixture.

```sh
just test-ui-e2e            # the tier (part of the gate's ACCEPTANCE phase for ui/ and full)
just test-ui-e2e-selftest   # put each known defect back and require the suite to go red
cd ui && npm run e2e -- surface-nav      # one file, by name substring
```

## Why it exists

Two defects in two days passed every suite this repo had.

**T-450 — the renderer could not load in a browser at all.** `cellrule.ts` compiled its pattern
predicates with `new Function` at module scope; `hk serve` sends `default-src 'self'` with no
`unsafe-eval`, so the module threw *while being evaluated* and the bundle never finished. Nothing in
`ui/src` imported it and node's test runner has no CSP — while T-441 had proved that same module's
shader against a CPU rule on 114 973 of 115 200 pixels. A correct proof about code that could never
run where the product runs.

**T-454 — the tile route's `503` backpressure reaching the user,** in a client that already had a
cap, an `AbortController` per request and measured cancellation.

Neither is reachable by a pixel rasterizer or a node unit test. One needs a CSP; the other needs
real concurrent fetches driven by a real render loop.

## The standard, and the trap

A browser test that asserts "it did not throw" is close to vacuous, and heavier machinery is no
excuse for a weaker claim. So this tier asserts the same *kinds* of things the unit tier does:

| Tier | Asserts on |
|---|---|
| T-441 | pixel histograms of the shader's output |
| T-442 | the requests the client builds |
| T-443 | the size, in device pixels, of every submitted quad |
| **here** | **pixel histograms of the composited page, the requests actually on the wire, and the readouts the user reads** |

Concretely: `surface-load` requires the canvas to hold ≥ 32 distinct colours, no single colour over
92 % of it, mean luma > 8, and a drawing buffer equal to the CSS box × dpr; `surface-nav` requires
every gesture to move the page's own per-viewport readout, peak on-the-wire tile concurrency to stay
inside the server's declared `cost.in_flight_limit`, zero `503`s, zero backpressure reported to the
user, non-zero cancellation, and the picture to differ from before.

Two of those deserve a note:

- **Concurrency is counted from CDP `Network` events, not from the client's own counter.** T-454 was
  a client whose bookkeeping said it was obeying the cap. Asking that bookkeeping whether it is
  correct would have passed then too.
- **The cap is read from the server** (`cost.in_flight_limit`), never restated here. The runner reads
  it *before any browser starts*, because asking for it while a browser holds four reads in flight
  gets a `503` — the harness would manufacture the condition it exists to detect.

## What's here

| File | What |
|---|---|
| `cdp.mjs` | Dependency-free Chrome DevTools Protocol driver. Finds a Chrome; launches headless. |
| `png.mjs` | Minimal PNG decoder + `census()`, the colour histogram the pixel assertions use. |
| `harness.mjs` | `Browser`/`Page`: navigation, console and exception capture, network recording with concurrency watches, gestures (drag, wheel, click, double-click), screenshots, named waits. |
| `backend.mjs` | Starts `hk serve` over the fixture; `assertRealCsp`; `tileCost`. |
| `run.mjs` | One backend, shared; one node process per `*.e2e.mjs`; prints the runtime of each. |
| `surface-load.e2e.mjs` | **T-450's guard.** |
| `surface-nav.e2e.mjs` | **T-454's guard.** |
| `selftest.mjs` | Reintroduces each defect in a scratch copy of `ui/src` and requires the suite to go red. |

## Dependencies: none new

The driver is ~120 lines over node 24's built-in `WebSocket`, and it runs **whatever Chrome the
machine already has**: the `ms-playwright` cache on the dev Mac, `google-chrome` / `chromium` on a
GitHub `ubuntu-latest` runner image (all preinstalled there), or `$CHROME`. No package was added to
`ui/package.json`, nothing is downloaded, and CI needs no browser-install step.

Playwright was the alternative the ticket offered. Its value is selector engines, auto-waiting,
cross-browser and a trace viewer — none of which this tier uses, since every assertion is a
`Runtime.evaluate`, a `Network` event or a screenshot. Its cost is a ~170 MB browser download per
machine plus a CI cache to keep warm. The trade we accept in exchange is that there is no
auto-waiting, so every wait here is an explicit named condition (`waitFor`, `waitForCanvas`,
`waitForSurfaceMounted`) — which is the honest form anyway; a sleep would be what eventually gets
this tier disabled.

**M8's field check** (`docs/11`) names Playwright as a new dependency for browser-driven E2E over the
live app on real hardware. It should use **this** harness rather than growing a second one: the parts
M8 needs are exactly the parts here — drive the real page, assert on what is drawn and requested —
and the only axis it adds is *which backend* (a live HackRF instead of `--replay`), which is one
argument to `startBackend`. Two harnesses would mean two CDP drivers, two sets of waits and two
answers to "did it render", and the T-441/T-450 lesson is precisely about two implementations of one
claim drifting apart. See `docs/11` M8.

## Runtime

Measured on the dev Mac, warm (`hk` already built, `npm ci` a no-op):

| Step | Time |
|---|---|
| `hk serve` up + surface history ready + cap read | ~1–2 s |
| `surface-load.e2e.mjs` | ~2 s |
| `surface-nav.e2e.mjs` | ~13–15 s |
| **`npm run e2e` total** | **~16–19 s** |
| `npm run e2e:selftest` (baseline + 2 faults) | ~80 s |

Cold, `just test-ui-e2e` also pays `cargo build -p hk-cli --bin hk` and `npm ci`.

## Ports, and the radio

Never binds 8788/8789/8899/8900 — the user's demo holds the real HackRF on 8899 — and
`startBackend` refuses those ports outright. `HK_STREAM_TCP` is pinned to an ephemeral port so the
stream server cannot take 8788 either. The backend is a `--replay` of a recording: nothing here can
tune a radio, and `surface-load` asserts that the page requested no `/api/control/` route at all.

## Findings this tier produced on day one

Recorded here rather than silently worked around, because they are the tier doing its job.

1. **`probeSurface` treats the tile route's `503` as fatal.** If `/api/tiles` is at its cap when the
   page loads, the page shows "The surface could not be addressed" and stops. Two tabs on `/surface`
   is enough to cause it. The route's `503` is documented backpressure asking the caller to retry —
   T-454's shape, at page load rather than during a pan. The runner drains before each file so the
   suite is not order-dependent, but the product still has no retry here.
2. **The client exceeds the server's cap under navigation.** Measured on the wire, reproducibly:
   peak **6** in flight against a declared limit of **4**, and 30–48 of ~1 400 tile requests refused
   `503`, every one of them surfaced to the user in the status line. Six is exactly Chrome's
   per-origin HTTP/1.1 connection limit, so 6 is a *lower bound* on what the client actually had
   outstanding. A plausible second mechanism, not excluded by this evidence: a client-side `abort()`
   does not release the *server's* in-flight slot — the server keeps counting a read whose tile
   production still holds the history lock — so even a client that obeys its own cap can be refused
   right after a fast pan. `surface-nav.e2e.mjs` is **red on `main`** until T-454 lands; the
   selftest reports T-454's fault as INCONCLUSIVE for exactly that reason, and should be re-run
   once the guard is green.
3. **Time zoom is structurally clamped on a seconds-long fixture** (the whole record is already on
   screen at the lattice's finest time level), which is why `surface-nav` exercises the time wheel
   but requires movement only on the frequency axis. Stated in the test rather than hidden.
