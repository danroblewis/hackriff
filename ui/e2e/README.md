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
inside the server's declared `cost.in_flight_limit`, and the backpressure bound derived below.

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
| `harness.mjs` | `Browser`/`Page`: navigation, console and exception capture, network recording with concurrency watches, gestures (drag, wheel **with real modifier bits**, click, double-click), screenshots, named waits. |
| `backend.mjs` | Starts `hk serve` over the fixture; `assertRealCsp`; `tileCost`. |
| `run.mjs` | One backend, shared; one node process per `*.e2e.mjs`; prints the runtime of each. |
| `surface-load.e2e.mjs` | **T-450's guard.** |
| `surface-nav.e2e.mjs` | **T-454's guard** — the in-flight cap and the AIMD contract. Plus **T-456's**: the four navigation gestures, and the modifier the browser actually delivered. |
| `surface-contention.e2e.mjs` | **T-454's bootstrap half**: a second tab must be able to open while the first saturates the route. |
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
| `hk serve` up + surface history ready + cap read | ~2 s |
| `surface-contention.e2e.mjs` | ~7 s |
| `surface-load.e2e.mjs` | ~2.5 s |
| `surface-nav.e2e.mjs` | ~22 s (8 s of it the deliberate steady-state window) |
| **`npm run e2e` total** | **~32 s** |
| `npm run e2e:selftest` (baseline + 5 faults) | ~4 min |

Cold, `just test-ui-e2e` also pays `cargo build -p hk-cli --bin hk` and `npm ci`.

## Ports, and the radio

Never binds 8788/8789/8899/8900 — the user's demo holds the real HackRF on 8899 — and
`startBackend` refuses those ports outright. `HK_STREAM_TCP` is pinned to an ephemeral port so the
stream server cannot take 8788 either. The backend is a `--replay` of a recording: nothing here can
tune a radio, and `surface-load` asserts that the page requested no `/api/control/` route at all.

## Where the `503` bound came from

Worth reading before touching `surface-nav`'s assertions, because the obvious bound is wrong in both
directions.

The first version asserted **zero** `503`s. That was right against the client this tier was written
for (27–48 refusals per run, peak 6 in flight against a cap of 4, every one surfaced). After T-454
landed it failed on **2 of ~1 850** — and relaxing it to 2 would have been the move this repo
refuses all week.

So the residual was measured, over eleven runs, to tell two live readings apart. **Discovery:**
T-454's fix is AIMD, and an AIMD controller finds its share of a *global* budget — shared by the
page's two viewports, its own abandoned reads, the bootstrap probe and any other tab — by being
refused. **Leakage:** a client `abort()` that fails to release the server's slot, which is what
T-454's abandoned-slot accounting addresses and which this tier had flagged as not excluded.

The measurement says discovery:

- peak on the wire is **exactly the cap, never above** (4/4, was 6/4);
- **every** refusal happens with the operating cap **at its ceiling** — never at 2 or 3, where
  leakage would also show. Same trace every run: 4 → 2 on the first refusal, +1, +1, back to 4;
- after the last viewport change the cap returns to the ceiling and **stays** there through ~3 700
  further requests over 25 s with **zero** refusals. A leak keeps leaking.

The replacement bound was then wrong once more, and the selftest caught it. It was a count with a
story — "AIMD cannot return to its ceiling more often than the gestures that push it off, so ≤ 1 per
gesture". A client with the halving **deleted** never leaves the ceiling, is refused 8–10 times
across the same nine gestures, and passes that bound *and* the steady-state one. A bound reasoned
from how the correct algorithm behaves does not constrain the incorrect one.

What is asserted now conditions the permission on the mechanism instead of counting it:

1. **peak ≤ the server's declared cap** — exact, no tolerance;
2. **zero refusals in 8 s of steady state**, with nothing moving the view (the window is sized to
   contain the measured 12–14 s return to the ceiling, so the client really is running at the cap);
3. **a refusal must be seen to halve the client's operating cap** — the AIMD contract itself. This
   is the sharp one: the refusals are permitted *because* they are a search, so the permission is
   void unless the halving is observed;
4. refusals ≤ viewport changes — a coarse guard against the pre-fix regime, safe to leave loose
   because (3) is sharp;
5. the client's `busyRefusals` must **equal** the wire's `503` count — the inversion of T-454's
   lesson that every mechanism counted the client while claiming something about the server.

`selftest.mjs` carries a fault for each: `t454-ignore-the-cap` (peak 13), `t454-forget-abandoned-slots`
(peak 5), `t454-never-back-off` (cap pinned at the ceiling, 10 refusals), and
`t454-probe-gives-up-on-503`.

## Findings this tier produced

Recorded here rather than silently worked around, because they are the tier doing its job.

1. **`probeSurface` treated the tile route's `503` as fatal** — found here before T-454 landed, when
   one file's browser left four reads in flight and the next file's page showed "The surface could
   not be addressed". Two tabs on `/surface` was enough. **Fixed by T-454** (bounded retry, doubling
   backoff) and now guarded by `surface-contention.e2e.mjs`, which opens a second tab *into* the
   first tab's whole-surface tile storm: measured 3–6 refusals on the second tab's probe, mounting
   anyway in 0.4–1.4 s and drawing 516–587 distinct colours. The run reports itself INCONCLUSIVE if
   the probe was never actually refused.
2. **The client exceeded the server's cap under navigation** — peak **6** against a limit of **4**,
   30–48 of ~1 400 requests refused, all surfaced. **Fixed by T-454**: peak is now exactly 4, and
   the residual is 1–2 discovery refusals, none of them surfaced as a failure. See the section
   above for how that was established rather than assumed.
3. **Time zoom is clamped until the record outgrows the zoom floor.** A pane may not magnify below
   `minCells` (16) level-0 cells — 16 × 1 s on this lattice — while the surface's time extent is
   however much of the recording `--replay --loop` has ingested so far, which grows in real time.
   Early in a run the two are the same size and the time axis is correctly clamped in both
   directions. T-456's test therefore **measures that premise from `/api/navigation` and
   `/api/tiles` and waits for it** (measured: ~48 s of record against a 16 s floor, after a ~24 s
   wait when that file is run alone; no wait at all in a full run, where it goes last) instead of
   assuming it from the run order. Stated in the test rather than hidden.
4. **A CDP wheel cannot answer an OS question.** T-456 needed to know whether ctrl+wheel reaches the
   page, and the harness can only dispatch at the renderer — macOS's Accessibility ctrl+scroll zoom
   consumes the event in the window server, where nothing in a browser can see it. So the test
   reports what the browser *did* deliver (ctrl arrives, `defaultPrevented`, no page zoom) and the
   product puts the time axis on **alt/option** for the reason the harness cannot test. Reading the
   modifier that arrived — rather than only that the view zoomed — is the whole point of the probe
   listener there.
