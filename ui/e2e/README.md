# `ui/e2e` — the browser tier (T-455)

Headless Chrome, driving `/surface.html` against a real `hk serve` over a recorded SigMF fixture.

```sh
just test-ui-e2e                    # the tier (part of the gate's ACCEPTANCE phase for ui/ and full)
just test-ui-e2e-selftest           # put each known defect back and require the suite to go red
just test-ui-e2e-selftest-timeout   # prove a hung spec is killed and reported red, not left to hang
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
| `run.mjs` | A bounded pool of **lanes** (`HK_E2E_CONCURRENCY`, default 3 — measured: 695.3 s sequential → 207.7 s), each with **its own `hk serve` on its own port range**; one node process per `*.e2e.mjs`, handed to whichever lane is free, longest spec first; prints the runtime of each, slowest first. A backend per lane rather than one shared one because `/api/tiles`' backpressure cap is counted per server — `surface-contention` asserts on it — so N browsers on one server would refuse each other's reads; per lane, a spec sees exactly the single-browser server it saw when this loop was sequential. `HK_E2E_CONCURRENCY=1` restores that sequential run exactly. Every spec runs under a per-file deadline, `HK_E2E_SPEC_TIMEOUT_MS` (default 600000 ms — well above the ~170 s the slowest file takes, and dearer still with three other lanes on the box): on expiry the spec is killed **whole-tree** — its node process, its Chrome, and any extra `hk serve` it started — and reported FAILED, never a pass (T-473). |
| `surface-load.e2e.mjs` | **T-450's guard**, on `/surface.html`. |
| `app-surface.e2e.mjs` | **T-445's guard**, on **`/` — the page the user actually opens.** The cutover put this renderer on the app's critical path and deleted the waterfall it replaces, so "the app comes up" stopped being a property of an additive preview. Different bundle (`--splitting`), different entry, different mount: passing `surface-load` says nothing about it. Also asserts the retired slots are absent, the rest of Explore is present, and that a drag moves the view while reaching no device route. Its own non-vacuity (T-466) is `t445-app-retired-slot-left-behind` in `selftest.mjs`. |
| `surface-nav.e2e.mjs` | **T-454's guard** — the in-flight cap and the AIMD contract. Plus **T-456's**: the four navigation gestures, and the modifier the browser actually delivered. |
| `surface-contention.e2e.mjs` | **T-454's bootstrap half**: a second tab must be able to open while the first saturates the route. |
| `surface-colour.e2e.mjs` | **T-470's guard**, on `/surface.html`: the same measured dB is the same colour at every zoom. Splits the surface, zooms **one** viewport through six states across four pyramid levels, and requires the other — same box, same level, same tiles, all checked from the page's own readout — to stay **byte-identical**. Its control is a mode switch, not a fault injection: see below. |
| `surface-region.e2e.mjs` | **T-458's guard**, on `/`: shift+drag marks out a region. Reads the `shiftKey` flag *on the `pointerdown` the canvas received* before concluding anything from the view, asserts the viewport does **not** move under a stroke, and re-states T-340's control over the new gesture. The file's header says `selftest.mjs` could not hold these faults because it built only the `/surface.html` bundle; **T-466** made `build()` compile the app bundle too and added `t458-region-modifier-ignored`/`t458-region-falls-through-and-pans`, each caught by this file alone. The header's third fault (the tap gate hard-wired open) no longer reddens anything here — recorded, not silently dropped, in `selftest.mjs`. |
| `canvas-journey.e2e.mjs` | **T-481: the whole user journey, in one flow, through the MOCK SDR** (`--device mock:…`, its own backend on its own port) — pan/zoom, retune, a tile off screen and back, then the backend killed underneath it. The standing guard for T-495/T-497/T-499. Its own findings are below. |
| `selftest.mjs` | Reintroduces each defect in a scratch copy of `ui/src` and requires the suite to go red. |
| `selftest-timeout.mjs` | T-473's own non-vacuity check: drives the real `run.mjs` against `selftest-fixtures/hang.e2e.mjs` (a spec that launches a Chrome and a backend, then hangs forever) via `HK_E2E_EXTRA_SPECS`, and requires the hang to be killed, reported red by name, and to leave no process behind — checked with `pgrep`, not assumed. |

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

**A wait answers about one frame; the read after it is a different frame.** `waitFor` returns a
boolean and how long it took, so a spec that then reads the element gets whatever the page says a
round trip later — and the product is under no obligation to still satisfy the condition. Where the
value matters, wait with the read that produces it (`traceMatching` in `app-trace.e2e.mjs`) or
bracket the two together (`heldObservation`, T-487). Measured cost of getting this wrong, on the
readout `app-trace` parses: two states both match "a slice with a peak" — a live frame, which needs
no tile, and a pyramid cell, which needs one — and a cold page passes through the first before the
tile route answers for the second, so the re-read lands on a peak-less cell on ~1 load in 40 (same
rate on `main` and on the tree that first tripped it).

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
| `app-surface.e2e.mjs` | ~3.5 s |
| `surface-region.e2e.mjs` | ~3.5 s (one browser and one app page shared by the file) |
| `surface-colour.e2e.mjs` | ~26 s (twelve settle-and-screenshot cycles; it compares whole panes) |
| `surface-nav.e2e.mjs` | ~22 s (8 s of it the deliberate steady-state window) |
| `canvas-journey.e2e.mjs` | ~100 s (its own mock-SDR backend; ~40 s of it is dwell — a tile must really be off screen while rows really arrive, and a killed server must really be given two windows to settle in) |
| **`npm run e2e` total** | **~60 s** |
| `npm run e2e:selftest`, one fault (e.g. `node e2e/selftest.mjs t445`) | ~9 min (baseline once + the whole suite again for the fault, ~4.5 min each) |
| `npm run e2e:selftest`, no filter (baseline + every fault in `FAULTS`) | scales with `FAULTS.length` — baseline once, then the whole suite per fault; run narrowed by name in practice |
| `node e2e/selftest.mjs --expected-only <fault…>` | baseline + each fault against ONLY the specs the faults name (T-846) — an iteration aid: proves the named guard goes red, cannot prove no other guard caught it instead |

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

**T-573's batch route made two of those faults invisible, and T-846 re-aimed them.** A
`GET /api/tiles/batch` carries up to 64 addresses, so a client ignoring its cap read `peak 1/4` on
the request count; the cap is per ADDRESS (the client charges a slot per address, the route takes a
producer slot per address), so (1) is now also asserted over `addressPeak(tileAsks(…))` — measured
3/4 on the correct client, 7/4 with the cap ignored. And the route no longer refuses on this fixture
at all (batch workers retire on a 503 instead of recording one; T-630's share clamps the ceiling on
every answer), so (3) judged an empty set and a client with the halving deleted stayed green. A
second test now puts ONE per-address `503` inside a real batch answer (a `fetch` wrapper, like
`live-edge`'s T-523 proxy) and requires the operating cap to fall across it: 4 -> 2 correct,
3 -> 3 with `t454-never-back-off`. The `t454-ignore-the-cap` fault itself moved to where the cap is
obeyed (`effectiveLimit` and `nextAddr`'s per-viewport share), because since T-630 the first answer
clamped the constructor's 64 back to the route's number.

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
4. **`startBackend` would adopt another worktree's server** (T-470). The readiness loop waits for
   *anything* to answer `/surface.html` on its port, and with up to four agents running at once the
   default 8791 is routinely already taken — so the suite drove **another worktree's bundle**,
   reporting on code the run never built. It cost three runs of a new guard failing against a page
   that contained none of the code under test, and it can fail the other way just as easily: a green
   about somebody else's build. `startBackend` now steps to the next free port before spawning
   (`freePort`), says so, and fails closed if none is free. The port was always internal — callers
   use the returned `origin` — so nothing else had to change.
5. **The obvious colour test asks the adjacent question, and the obvious control is a flake.**
   T-470's claim is *"the same measured dB is the same colour at every zoom"*, but zooming changes
   which pyramid level answers, so a cell's measurement legitimately changes with zoom and "zoom in,
   check the pixels match" would assert something false. Hence the split-pane form: the viewport that
   did **not** move is showing the same numbers from the same tiles, so its pixels may not move at
   all. Three further things were measured rather than assumed, each after the test caught itself:
   zooming *in* from the opening view changes no level (the panes open at the lattice's finest), so
   the run zooms **out and back**; a pane whose stand-ins are still resolving repaints for reasons
   that have nothing to do with colour, so residency is read from `.hk-surface-counts` and pinned at
   every step; and **switching auto-contrast on is not a reliable fault generator** — it tracks the
   union over *every* tile on screen, and the pane deliberately not moving usually pins both ends, so
   a zoom of the other pane moved the range on 3 runs in 4 and 0 % on the fourth. The control is
   therefore the *sensitivity* of the comparison — switching mode changes the range and nothing else,
   and repaints 14–23 % of the very pixels that stayed byte-identical through six zooms. That is
   deterministic, and it is the stronger statement: not "the fault can appear" but "had the range
   moved at all, every assertion would have failed".
6. **A CDP wheel cannot answer an OS question.** T-456 needed to know whether ctrl+wheel reaches the
   page, and the harness can only dispatch at the renderer — macOS's Accessibility ctrl+scroll zoom
   consumes the event in the window server, where nothing in a browser can see it. So the test
   reports what the browser *did* deliver (ctrl arrives, `defaultPrevented`, no page zoom) and the
   product puts the time axis on **alt/option** for the reason the harness cannot test. Reading the
   modifier that arrived — rather than only that the view zoomed — is the whole point of the probe
   listener there.

## The canvas journey's own findings (T-481)

Recorded here rather than worked around, because they are the tier doing its job. Every number below
was measured against `main` at the time of writing, on `--device mock:fixtures/hackrf/2026-09-13/fm_100p8M_2p4M…`.

1. **The client hammers a dead server for as long as the page is open.** With `hk serve` SIGKILLed
   under a live page: **183 failed requests in the first 5 s and 179 in the next**, flat, over
   repeated runs (172–183 then 170–179). No decay, no ceiling. This is **T-499's second half** — the
   render/retry loop — and it is the one assertion in this file that is red on `main` today. The
   *first* half did not reproduce: **0 magenta pixels** before the kill and 0 after, with texture
   uploads at 0 and 0.00 % of the pane repainting between the two windows. So on this backend the
   loop is on the wire, not on the screen. The two halves are asserted separately for exactly this
   reason.
2. **The live edge draws THE grey over rows the server holds.** Over a viewport the server reports
   **100 % observed**, fully resident (`0 coarse stand-ins · 0 pending`), the grey share of the
   newest 40 % of the pane is **17–31 %**, pulsing frame to frame (measured single frames: 0.0, 3.6,
   7.4, 9.4, 12.3, 18.7, 53.7 %). The profile by vertical tenth puts every grey pixel in the newest
   tenths and none below them — `100% 53% 35% 0% 0% 0% 0% 0% 0% 0%` in one run, `15% 0% 0% …` in
   another. Grey is the one colour that may only mean *the radio never looked*, and these rows were
   recorded and are served. It is **reported and not asserted**: the amplitude varies by a factor of
   fifty between runs, so a threshold over it would be a coin toss rather than a guard. Every grey
   claim in the file is therefore made **below** that zone (`LIVE_EDGE_ZONE`), where the same
   measurement reads 0.0 % in every run.
3. **Below the live edge, grey tracks the coverage map almost exactly.** Zoomed out to 9x the tuned
   window: the pane draws **88–90 %** THE grey where the server reports **88.8 %** unobserved. That
   agreement only appears once two things are controlled, and both were found the hard way:
   - **Ask the server at the level the PANE drew at.** A coarse cell is observed if anything in it
     was (docs/16 §8.5a), so the same band compared at 256 server cells against a pane drawing 8
     gave 0.0 % and then 44.1 % on consecutive runs for the same 88.8 %. The pane states its level;
     the route states the lattice; the query is built from both.
   - **Exclude `unknown` from the denominator.** On a young backend everything before
     `horizon.oldest_record_s` is `unknown` — 1383 of 3688 cells over a window the server had only
     partly lived through — and `unknown` is neither grey nor a level (T-423). Counting it either way
     makes the comparison meaningless. Rows finer than a level-0 cell manufacture it too: the same
     20 s asked as 64 rows returns 1536 `unknown` and as 8 rows returns none.
4. **T-495 and T-497 did not reproduce through the mock SDR.** Stated plainly rather than papered
   over, and the assertions are left in their honest form:
   - **Retune (T-497):** after a press on the persistent per-pane control, the front end moved
     100.800 → 100.707 MHz with a rate change (2.400 → 2.000 MHz) in between — a real re-plumb — and
     **242–244 spectrum rows arrived at the NEW centre in the following 10 s, 0 at the old**, with
     0 socket closes and 0 errors, on every run. If T-497 is real, its cause is not in this path on
     this backend.
   - **Off screen and back (T-495):** with a freshly-live tile (the one the retune created), 13 s
     off screen during which the server recorded 256/256 cells observed, and 12 s after the return
     for those rows to scroll out of the live-edge zone — **0.00 % grey before and 0.00 % after**.
   Both assertions are written to fail if the property breaks, and both are worth keeping for that.
5. **Two harness traps, paid for.** `location.href` to the same document with only a changed `#hash`
   is a same-document navigation: no reload, no `load` event, and `goto` reports `timeout` for a page
   that is working perfectly — so `reopen` carries a unique query. And the surface opens on bounds
   padded well outside the observed region (98.6–120.8 MHz for a capture at 99.6–102.0 MHz), so a
   wheel about the canvas centre converges on 109.7 MHz and never reaches the tuned window however
   many notches it gets. Navigation here therefore pans as well as zooms, with **Hz-per-drag-pixel
   measured on the page** rather than re-derived from `view.ts`.
6. **Instruments are checked against a known negative.** Test 2's claim is "≥ 20 rows at the new
   centre in 10 s"; test 4 reads the same counter with the server killed and requires **0**, having
   first confirmed it was non-zero moments earlier — so the passing number in test 2 is one that has
   been seen to fail. The magenta predicate is likewise checked against the surface's own `unknown`
   ink (rgb 112, 77, 133 — the one legitimately magenta mark) and against a ramp colour, in the file
   that defines it.
