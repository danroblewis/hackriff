// **T-476: the persistent per-pane retune control, confirmed in a real browser.**
//
// The ticket exists because a control *disappeared*, and the whole fix is that it is now on screen
// at all times with a sentence attached. That is a claim about the DOM of a running page, and
// `ui/test/` has no document at all — the node tier can prove `paneRetuneOffer` produces an offer
// for every pane state, and proves exactly nothing about whether a user can see or press one.
//
// ——— THE TRAP THIS FILE IS WRITTEN AGAINST ———
//
// From the brief, and from T-441/T-448/T-454 before it: **a control that renders is not a control
// that retunes to the viewport it is showing.** So "there is a button" is not the assertion here.
// Each test reads the control's own *sentence* and ties it to the viewport the page says it is
// looking at — the `.hk-surface-where` readout — and the gesture tests read what went **on the
// wire** rather than what the page looks like afterwards.
//
// ——— TWO BACKENDS, BECAUSE ONE OF THEM CAN ONLY PROVE HALF ———
//
// Tests 1–4 run on the tier's shared `hk serve --replay`. A replay reports **no frequency grid** and
// is not live, so every control on that page is correctly stated-and-disabled. That proves the
// disabled half — the control is there, it says why, and pressing it reaches no device route — and
// proves *nothing whatever* about the enabled half. Stopping there would be this repo's recurring
// failure exactly: a sound proof of an adjacent question.
//
// Test 5 therefore brings up a second `hk serve` over the **same fixture behind the mock SDR device**
// (`--device mock:…`), which reports a HackRF-class grid, an active capture window, and takes a
// retune. CLAUDE.md's rule for this tier is that e2e drives the system THROUGH the device interface,
// and the mock is that device — so the enabled press is driven for real, in a real browser, and the
// real radio is never touched.
//
// ——— NON-VACUITY, MEASURED ———
//
// Two faults were put back into `ui/src`, rebuilt, and run against this file. Measured, not assumed:
//
//   the T-444 containment trigger restored (the control vanishes when
//   the viewport is contained in a tuned window)                          -> tests 5 and 6 RED
//   `readoutOf` passes no action to the rows (the control never reaches
//   the chrome at all)                                                    -> ALL SIX RED
//
// The first is the defect this ticket is about, put back verbatim, and **only tests 5 and 6 catch
// it** — because only the mock-SDR backend ever reports a capture window for a viewport to be inside
// of. On the `--replay` backend `windows` is empty, so nothing is ever "covered" and the trigger
// cannot fire. That is exactly why this file pays for a second backend, and it is the measurement
// that shows tests 1–4 alone would have been a green suite over the shipped defect.
//
// Recorded here rather than left in `ui/e2e/selftest.mjs`, for the reason `surface-region.e2e.mjs`
// gives: its `build()` compiles only the `/surface.html` bundle and this file drives the APP at `/`.
import test, { after } from "node:test";
import assert from "node:assert/strict";
import { Browser } from "./harness.mjs";
import { startBackend } from "./backend.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;

/** The per-pane control and its sentence, in CSS. One spelling each, used by the waits and presses. */
const PANE_ROW = '.hk-surface-viewport[data-viewport="pane"]';
const PANE_ACTION = `${PANE_ROW} .hk-surface-action:not([hidden])`;
const PANE_WHY = `${PANE_ROW} .hk-surface-why`;

/** Every device route. A press on a replay must reach none of them, and neither may a gesture. */
const DEVICE = /\/api\/control\/(center|rate|gains|bias_tee|baseband_filter)$/;

/**
 * **The zoom gesture this file uses: SHIFT-held, a frequency-only zoom. T-472 is why.**
 *
 * Everything this file zooms for is a claim about **frequency** — "deep inside the tuned window"
 * (test 2), "the viewport is now inside the window in force" (test 5). A plain wheel is the *uniform*
 * gesture, and since T-472 it stops as soon as **either** axis reaches a bound, so the aspect ratio
 * cannot drift out from under a gesture that promised to scale both equally. On a young record the
 * time axis is pinned before the first wheel, and a plain wheel then correctly moves **neither**
 * axis — which turned test 5's premise into a no-op outright ("the viewport never got inside the
 * tuned window") and left test 2 green off a single step while its comment still claimed three
 * orders of magnitude.
 *
 * Shift is untouched by the lock and is the instrument these claims actually want. The claims are
 * unchanged; only the gesture is. (Where a *plain* wheel is the subject — test 4's "no gesture
 * reaches a device route" — it stays, alongside the modified ones, because there the point is the
 * vocabulary rather than the travel.)
 */
const ZOOM = { shift: true };
const deviceCalls = (page) =>
  page.requests.filter((r) => DEVICE.test(new URL(r.url).pathname)).map((r) => `${r.method} ${new URL(r.url).pathname}`);

/**
 * Every viewport row the page is drawing, as the user would read it: which viewport, where it is
 * looking, and **its control** — present or not, enabled or not, and the sentence beside it.
 *
 * Read from the DOM rather than from any client-side bookkeeping: T-454's defect was a client whose
 * own counter said it was behaving.
 */
const ROWS = `JSON.stringify([...document.querySelectorAll('.hk-surface-viewport')].map((v) => {
  const b = v.querySelector('.hk-surface-action');
  const why = v.querySelector('.hk-surface-why');
  const shown = (el) => !!el && !el.hidden && el.offsetParent !== null;
  return {
    id: v.querySelector('.hk-surface-id')?.textContent ?? '',
    viewport: v.getAttribute('data-viewport'),
    where: v.querySelector('.hk-surface-where')?.textContent ?? '',
    hasButton: shown(b),
    label: b?.textContent ?? null,
    disabled: b ? b.disabled : null,
    why: shown(why) ? (why.textContent ?? '') : null,
    title: b?.title ?? null,
  };
}))`;

const rows = async (page) => JSON.parse(await page.eval(ROWS));
const panes = (rs) => rs.filter((r) => r.viewport === "pane");

/** One app page for the whole file, opened lazily and cached — the rejection too (T-455's idiom). */
let opening = null;
const app = () => (opening ??= openApp());
after(async () => { (await opening?.catch(() => null))?.browser?.close(); });

async function openApp() {
  const browser = await Browser.open();
  const page = await browser.page();
  page.browser = browser;
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load", "the app page never fired load");
  await page.waitFor("the app shell to mount its surface slot",
    `!!document.querySelector('.sf-canvas')`, { timeoutMs: 15000 });
  await page.waitFor("the app's surface to finish addressing",
    `(document.querySelector('.sf-note')?.textContent ?? "").length > 0`, { timeoutMs: 60000 });
  const note = (await page.$text(".sf-note")) ?? "";
  assert.ok(!/could not be addressed|WebGL2 is unavailable/.test(note), `the surface refused to mount: ${note}`);
  await page.waitFor("a viewport readout to exist", `document.querySelectorAll('.hk-surface-viewport').length > 0`);
  // The control is re-derived per frame from `GET /api/navigation`'s grid, which arrives on a poll.
  // Waiting for the row rather than for a timer: "the button exists" is the thing under test.
  await page.waitFor("the per-pane retune control to be on the page",
    `!!document.querySelector('${PANE_ACTION}')`, { timeoutMs: 20000 });
  await page.frames(3);
  return page;
}

/** A point well inside the canvas and clear of the 110-device-px map strip along its bottom. */
async function centre(page) {
  const r = await page.$rect(".sf-canvas");
  assert.ok(r && r.w > 300 && r.h > 260, `the canvas has no usable box: ${JSON.stringify(r)}`);
  return { x: r.x + r.w * 0.5, y: r.y + r.h * 0.35, rect: r };
}

// ---------------------------------------------------------------------------

test("1. the control is on EVERY pane row, with a sentence, and never on the map", async () => {
  const page = await app();
  const rs = await rows(page);
  assert.ok(panes(rs).length >= 1, `no pane row at all: ${JSON.stringify(rs)}`);
  for (const r of panes(rs)) {
    assert.equal(r.hasButton, true, `pane ${r.id} has no retune control: ${JSON.stringify(r)}`);
    assert.equal(r.label, "Retune", `pane ${r.id}'s control is labelled ${JSON.stringify(r.label)}`);
    // The sentence, not merely a button: a disabled control with no reason is the same silence the
    // vanishing control was. It is shown AND is the tooltip, so it is readable either way.
    assert.ok(r.why && r.why.length > 20, `pane ${r.id}'s control says nothing: ${JSON.stringify(r.why)}`);
    assert.equal(r.title, r.why, "the tooltip and the visible sentence must be the same statement");
    // Honesty: enabled means it names a destination; disabled means it names a reason.
    if (r.disabled) {
      assert.match(r.why, /tunable range|survey overview|frozen behind the growing edge/,
        `pane ${r.id} is disabled without saying why: ${r.why}`);
    } else {
      assert.match(r.why, /^Retune to [\d.]+ MHz at [\d.]+ MHz span/, `an enabled control must name where: ${r.why}`);
    }
  }
  for (const r of rs.filter((x) => x.viewport === "minimap")) {
    assert.equal(r.hasButton, false, "the map got a retune control: it is not a viewport you look through");
  }
});

test("2. THE TICKET: zooming DEEP INSIDE the tuned window keeps the control — it used to vanish there", async () => {
  const page = await app();
  const at = await centre(page);
  const before = await rows(page);
  const b0 = panes(before)[0];
  assert.equal(b0.hasButton, true, "the run needs a control to start with, or it proves nothing");

  // Zoom in hard about the middle of the pane. A negative deltaY is zoom IN (`zoomFactor` is
  // `exp(px * 0.0015)`), and eight of them is ~3 orders of magnitude — comfortably inside whatever
  // the tuned window is, which is the condition that used to erase the control.
  //
  // **SHIFT-held, and T-472 is why** (see [[ZOOM]]). "Deep inside the tuned window" is a claim about
  // FREQUENCY, and shift is the gesture that makes it one: a plain wheel is the uniform zoom, which
  // since T-472 stops as soon as *either* axis reaches a bound, so on a young record — where the
  // time axis is pinned before the first wheel — it would travel a step or two and stop, leaving
  // this test's premise ("~3 orders of magnitude") quietly false while it still went green.
  for (let i = 0; i < 8; i++) await page.wheel(at, -400, ZOOM);
  await page.frames(4);

  const after = await rows(page);
  const a0 = panes(after)[0];
  // NON-VACUITY, first half: the zoom actually happened. Without this, a page that ignored the wheel
  // entirely would pass the assertion below for the wrong reason.
  assert.notEqual(a0.where, b0.where, `the wheel did not move the viewport at all: ${b0.where}`);
  assert.match(a0.where, /MHz ± /, `the readout is not a viewport window: ${a0.where}`);

  // The claim.
  assert.equal(a0.hasButton, true, "the control disappeared on a zoomed-in viewport — this is T-476 itself");
  assert.ok(a0.why && a0.why.length > 20, `the control went silent on a zoomed viewport: ${JSON.stringify(a0.why)}`);
  // NOTE, deliberately not asserted here: on THIS backend (a `--replay`) the front end reports no
  // frequency grid at all, so the sentence is the same stated refusal at every zoom. That the
  // sentence TRACKS the viewport is test 5's claim, against the mock SDR, which does report one.
  // Asserting it here would be an assertion about the fixture, not about the control.

  // …and zooming back out keeps it too: the control is not a state you can fall out of.
  for (let i = 0; i < 10; i++) await page.wheel(at, 400, ZOOM);
  await page.frames(4);
  const out = panes(await rows(page))[0];
  assert.equal(out.hasButton, true, "the control vanished on the way back out");
  assert.ok(out.why && out.why.length > 20, `the control went silent zoomed out: ${JSON.stringify(out.why)}`);
});

test("3. a split gives each viewport its OWN control, and moving one does not disturb the other", async () => {
  const page = await app();
  const nPanes = panes(await rows(page)).length;
  await page.click(`[...document.querySelectorAll('.sf-actions button')].find((b) => /Split/.test(b.textContent))`);
  await page.frames(4);
  const rs = await rows(page);
  assert.equal(panes(rs).length, nPanes + 1, `the split did not add a viewport: ${JSON.stringify(rs.map((r) => r.id))}`);
  for (const r of panes(rs)) {
    assert.equal(r.hasButton, true, `pane ${r.id} lost its control after the split`);
    assert.ok(r.why && r.why.length > 20, `pane ${r.id}'s control says nothing after the split`);
  }

  // A split opens both viewports on the IDENTICAL box (T-442), so they start out agreeing. Move one
  // and they must stop agreeing about WHERE while both keep a control — one control per viewport,
  // not one control wearing N rows. (That each control names *its own* viewport's frequency is
  // asserted in test 6, on the mock backend, where the front end reports a grid to plan against.)
  const at = await centre(page);
  const r0 = await page.$rect(".sf-canvas");
  const before = panes(await rows(page));
  await page.drag({ x: r0.x + r0.w * 0.25, y: at.y }, { x: r0.x + r0.w * 0.12, y: at.y }, 8);
  await page.frames(4);
  const after = panes(await rows(page));
  assert.deepEqual(after.map((r) => r.id), before.map((r) => r.id), "the split's panes changed identity under a drag");
  assert.notEqual(after[0].where, after[1].where,
    `the two viewports still report the same window, so nothing was moved: ${after[0].where}`);
  assert.equal(after[1].where, before[1].where, "moving one viewport moved the other");
  for (const r of after) {
    assert.equal(r.hasButton, true, `pane ${r.id} lost its control when a viewport moved`);
    assert.ok(r.why && r.why.length > 20, `pane ${r.id}'s control went silent when a viewport moved`);
  }
});

test("4. THE CONTROL (T-340/T-407): no gesture presses it, and a press on a replay reaches no device route", async () => {
  const page = await app();
  const at = await centre(page);
  assert.deepEqual(deviceCalls(page), [], "the app commanded the front end just by opening");

  // Drags the width of the pane, wheels at every scale and in **every modifier T-456 defines** — the
  // whole pointer vocabulary, over a canvas that now has a permanently armed control sitting beside
  // it. A gesture must still be a gesture.
  //
  // The bare wheels stay (here the subject is the wheel *path*, not how far it travels), but they
  // are no longer the whole vocabulary: since T-472 a plain wheel can legitimately be a no-op at a
  // bound, and a vocabulary made only of those would be a negative claim over gestures that did
  // nothing. Shift and alt each move one axis whatever the other is doing, so they always travel.
  for (const dx of [-0.4, 0.4, -0.05, 0.05]) {
    await page.drag({ x: at.x, y: at.y }, { x: at.x + at.rect.w * dx, y: at.y + 40 }, 10);
  }
  for (const d of [-600, 600, -60, 60]) await page.wheel(at, d);
  for (const d of [-600, 600]) {
    await page.wheel(at, d, { shift: true });
    await page.wheel(at, d, { alt: true });
  }
  await page.frames(4);
  assert.deepEqual(deviceCalls(page), [], "a pan or a wheel reached a device route");

  // Now the discrete act, for real, in the browser. The fixture is a recording, so the client's own
  // gate (`mayRetune`) refuses before anything is posted — which is exactly what must be observable.
  const before = panes(await rows(page))[0];
  await page.click(`document.querySelector('${PANE_ACTION}')`);
  await page.frames(4);
  assert.deepEqual(deviceCalls(page), [], "a replay posted a tuning: the device is not there to move");
  // NON-VACUITY: the press landed on a real, enabled control and the page answered it — otherwise
  // "no device route" is the trivially true statement about a button that is not there.
  if (before.disabled === false) {
    // `#toast` is the app shell's one toast element, and `applyDeviceAction` writes `NOT_LIVE_TEXT`
    // into it for exactly this case (`ui/src/app/centre/view.ts`).
    const toast = (await page.$text("#toast")) ?? "";
    assert.ok(/not live \(replay\)/i.test(toast),
      `an enabled control was pressed on a replay and the page said nothing: ${JSON.stringify(toast)}`);
  } else {
    assert.ok(before.why && before.why.length > 20,
      "the control was disabled and also silent, which is the failure this ticket is about");
  }
  assert.deepEqual(page.exceptions, [], "uncaught exception while pressing the retune control");
});

// ---------------------------------------------------------------------------
// 5. The enabled half, against the MOCK SDR — where a retune can actually happen
// ---------------------------------------------------------------------------
//
// Tests 1–4 run on the shared `--replay` backend, which reports no frequency grid and is not live.
// Every control there is correctly stated-and-disabled, which proves the *disabled* half and nothing
// at all about the enabled one — and "the control renders" was named in the brief as precisely the
// adjacent question this file must not answer instead of the real one.
//
// So this test brings up a second `hk serve` over the SAME fixture behind the **mock SDR device**
// (`--device mock:…`): a HackRF-class grid, an active capture window at 100.8 MHz ± 1.2 MHz, and a
// front end that takes a retune. That is CLAUDE.md's own rule for this tier — e2e drives the system
// through the device interface, and the mock is the device — and it means the real radio is never
// touched.
//
// The claim is the ticket's, and it is asserted on the wire: **a viewport that is FULLY INSIDE the
// tuned window retunes to itself, to the centre the control's own sentence named.**

/**
 * The mock-SDR page, opened once and shared by tests 5 and 6 — the tier's own caching idiom, and
 * here it also means one `hk serve` and one browser rather than two of each.
 */
let openingMock = null;
const mockApp = () => (openingMock ??= openMock());
after(async () => {
  const m = await openingMock?.catch(() => null);
  m?.browser?.close();
  m?.backend?.stop();
});

async function openMock() {
  const backend = await startBackend({ port: 8795, mockDevice: true });
  let browser;
  try {
    browser = await Browser.open();
    // Wait for the mock to have put something in the coverage map, over HTTP. The tier's own
    // `waitForSurfaceHistory` opens a second browser and reloads `/surface.html` in a loop to learn
    // the same fact; that is the right tool for the shared backend, which every file depends on, and
    // an expensive one to pay again here. The page opens on observed coverage (`openingWindow`), so
    // without this the viewport opens on the whole 6 GHz surface and never reaches the tuned window.
    const covered = await waitForCoverage(backend, 90000);
    const page = await browser.page();
    assert.equal(await page.goto(`${backend.origin}/#token=${backend.token}`), "load");
    await page.waitFor("the app shell to mount its surface slot", `!!document.querySelector('.sf-canvas')`, { timeoutMs: 15000 });
    await page.waitFor("the app's surface to finish addressing",
      `(document.querySelector('.sf-note')?.textContent ?? "").length > 0`, { timeoutMs: 60000 });
    await page.waitFor("the per-pane control to exist", `!!document.querySelector('${PANE_ACTION}')`, { timeoutMs: 30000 });
    await page.frames(3);
    return { page, browser, backend, covered };
  } catch (e) {
    browser?.close();
    backend.stop();
    throw e;
  }
}

/** The capture window the mock front end reports it is using, right now. */
async function tunedWindow(backend) {
  const nav = await get(backend, "/api/navigation");
  const w = nav.windows?.[0];
  assert.ok(w, `the mock reported no capture window: ${JSON.stringify(nav.windows)}`);
  return { loHz: w.f_lo_hz, hiHz: w.f_hi_hz, spanHz: w.span_hz, centerHz: w.center_hz };
}

test("5. THE TICKET, enabled: a viewport INSIDE the tuned window retunes to where the control SAYS", async (t) => {
  const { page, backend, covered } = await mockApp();
  t.diagnostic(`mock SDR coverage after ${covered.ms} ms: ${covered.observed} observed cells`);
  const pane0 = async () => panes(await rows(page))[0];

  // **Zooming is what makes it achievable, and that is the ticket in one gesture.** The view opens
  // on whatever coverage exists, which can be wider than one live window — correctly "survey
  // overview, and no single retune covers it". Wheel in until the viewport is both takeable and
  // inside the window in force, recording how the control read on the way, so the transition is
  // evidence rather than a precondition.
  //
  // The wheel is SHIFT-held ([[ZOOM]]): getting *inside a frequency window* is a frequency claim,
  // and a plain wheel is the uniform zoom, which T-472 stops as soon as either axis reaches a bound.
  // On this mock-SDR backend the time axis is pinned from the first frame, so a plain wheel moved
  // neither axis and this loop ran its twenty iterations without the viewport ever narrowing.
  const at = await centre(page);
  const w0 = await tunedWindow(backend);
  const inside = (v) => v.loHz >= w0.loHz && v.hiHz <= w0.hiHz;
  const MHz = (hz) => (hz / 1e6).toFixed(3);
  const span = (v) => `${MHz(v.loHz)}-${MHz(v.hiHz)} MHz`;
  const seen = [];
  let row = await pane0(), view = viewportOf(row.where);
  for (let i = 0; i < 20 && !(row.disabled === false && inside(view)); i++) {
    seen.push(`${row.where} -> ${row.why}`);
    await page.wheel(at, -400, ZOOM);
    await page.frames(3);
    row = await pane0();
    view = viewportOf(row.where);
  }
  // Both failures below name the window that was WANTED **and the one that was reached**, plus
  // whether the viewport narrowed at all across the run. A failure that prints only the target says
  // nothing about whether the gesture was refused, was too small, or overshot — and three merges
  // this session were slowed by exactly that.
  const first = viewportOf(seen.length ? seen[0].split(" -> ")[0] : row.where);
  const got = `wanted ${span(w0)}, viewport is ${span(view)} (opened at ${span(first)}, ` +
    `${seen.length} wheel step(s))`;
  // Printed on every run, not only on failure: "zooming is what makes it achievable" is this test's
  // premise, and a run that arrived inside the window with ZERO wheel steps would satisfy every
  // assertion below while demonstrating nothing about the zoom.
  t.diagnostic(`into the tuned window: ${got}`);
  // The precondition the ticket is about, ESTABLISHED rather than assumed: the viewport is now fully
  // inside the window in force, which under T-444 is exactly when the control was hidden.
  assert.ok(inside(view),
    `the viewport never got inside the tuned window — ${got}:\n  ${seen.join("\n  ")}`);
  assert.equal(row.disabled, false,
    `the control is not takeable on a live viewport inside the tuned window — this is T-476 itself. ` +
    `${got}:\n  ${seen.join("\n  ")}`);
  // The wide opening view legitimately said "survey overview"; what matters is that it did not STAY
  // that way and did not vanish on the way in either.
  for (const line of seen) assert.match(line, / -> .{20,}/, `a viewport on the way in said nothing: ${line}`);

  // What the control SAYS it will do, read off the page exactly as a user reads it.
  const said = row.why.match(/^Retune to ([\d.]+) MHz at ([\d.]+) MHz span/);
  assert.ok(said, `the enabled control does not name a destination: ${row.why}`);
  const saidCenterHz = Number(said[1]) * 1e6, saidSpanHz = Number(said[2]) * 1e6;
  // The resolution argument the ticket is founded on, measured rather than asserted: the capture it
  // offers is NARROWER than the one already in force over this same spectrum.
  assert.ok(saidSpanHz < w0.spanHz,
    `a contained viewport's retune must buy a narrower capture: ${saidSpanHz} vs ${w0.spanHz}`);
  assert.match(row.why, /narrower than the [\d.]+ MHz capture in force/,
    `the control does not say what it buys: ${row.why}`);

  // **The press, and the device's own sequencing.** `applyDeviceAction` posts the covering rate and
  // then the centre; a rate change RE-PLUMBS the capture, so the centre can land inside the settle
  // gap and come back `device_busy` — one capture at a time, which is the rule, not a fault. The
  // right answer to a busy radio is the one the client already gives: say so, and let the user press
  // again. So this presses again, and asserts on where the FRONT END ends up.
  let w1 = w0;
  for (let attempt = 0; attempt < 5; attempt++) {
    await page.click(`document.querySelector('${PANE_ACTION}')`);
    await page.frames(4);
    for (let i = 0; i < 20; i++) {
      w1 = await tunedWindow(backend);
      if (Math.abs(w1.centerHz - saidCenterHz) < 100) break;
      await new Promise((r) => setTimeout(r, 250));
    }
    if (Math.abs(w1.centerHz - saidCenterHz) < 100) break;
  }

  // (1) ON THE WIRE: the press reached the device through T-343's gate, and nothing else did.
  const posts = page.requests.filter((r) => DEVICE.test(new URL(r.url).pathname));
  assert.ok(posts.length > 0, "the press reached no device route at all");
  assert.deepEqual([...new Set(posts.map((r) => new URL(r.url).pathname))].sort(),
    ["/api/control/center", "/api/control/rate"],
    `the press touched a route it should not: ${JSON.stringify(posts.map((r) => r.url))}`);

  // (2) AT THE FRONT END: the window in force is the one the CONTROL NAMED, it is narrower than the
  //     one before, and it still contains the viewport the user was looking at. This is the claim —
  //     not that a button exists, and not that the client's own bookkeeping agrees with itself.
  assert.ok(Math.abs(w1.centerHz - saidCenterHz) < 100,
    `the radio went to ${w1.centerHz}, and the button said ${saidCenterHz} (toast: ${await page.$text("#toast")})`);
  assert.equal(w1.spanHz, saidSpanHz, "the capture width is not the one the control named");
  assert.ok(w1.spanHz < w0.spanHz, `the capture did not get narrower: ${w0.spanHz} -> ${w1.spanHz}`);
  // `view` was parsed from the page's own readout, which states MHz to 3 dp — so it is only known to
  // about ±500 Hz on the centre and again on the half-width. The tolerance is that rounding, not
  // slack in the claim: exact containment is `shortfallHz === 0`, asserted on the plan in
  // `ui/test/surface-retune.test.ts`.
  const READOUT_TOL_HZ = 3e3;
  assert.ok(view.loHz >= w1.loHz - READOUT_TOL_HZ && view.hiHz <= w1.hiHz + READOUT_TOL_HZ,
    `the retune left the viewport outside the new window: view ${view.loHz}-${view.hiHz} vs ${w1.loHz}-${w1.hiHz}`);
  assert.deepEqual(page.exceptions, [], "uncaught exception during the retune");
});

test("6. PER PANE: two viewports, two controls, each naming ITS OWN window", async () => {
  const { page } = await mockApp();
  const at = await centre(page);
  await page.click(`[...document.querySelectorAll('.sf-actions button')].find((b) => /Split/.test(b.textContent))`);
  await page.frames(4);
  // Move ONE of them along frequency. A split opens both on the identical box (T-442), so until one
  // moves they legitimately name the same destination.
  const r0 = await page.$rect(".sf-canvas");
  await page.drag({ x: r0.x + r0.w * 0.25, y: at.y }, { x: r0.x + r0.w * 0.12, y: at.y }, 8);
  await page.frames(4);

  const rs = panes(await rows(page));
  assert.equal(rs.length, 2, `expected two viewports: ${JSON.stringify(rs.map((r) => r.id))}`);
  const named = [];
  for (const r of rs) {
    assert.equal(r.hasButton, true, `pane ${r.id} has no control`);
    const m = (r.why ?? "").match(/^Retune to ([\d.]+) MHz at ([\d.]+) MHz span/);
    assert.ok(m, `pane ${r.id}'s control names no destination: ${r.why}`);
    const centerHz = Number(m[1]) * 1e6, spanHz = Number(m[2]) * 1e6;
    const view = viewportOf(r.where);
    // **The claim: each control plans for the viewport ON ITS OWN ROW.** The capture it names must
    // COVER the window the same row is reporting — which is `retunePlan`'s whole contract, and ties
    // the number on the button to the picture beside it, per pane, independently. A control that
    // planned for "the active pane" would put one centre on both rows; that is caught here for
    // whichever row is not the active one, and again by the inequality below.
    //
    // Containment rather than "centre + span/4": the off-DC dodge is BOUNDED by the selection
    // staying inside the window, so a viewport nearly as wide as the narrowest achievable capture
    // gets only a few kHz of offset, not span/4. Asserting the quarter-band here would be asserting
    // something `retunePlan` never promised (it is asserted where it does hold — a narrow selection
    // — in `ui/test/surface-retune.test.ts`).
    const TOL_HZ = 3e3;   // the readout states MHz to 3 dp; this is that rounding, not slack.
    assert.ok(centerHz - spanHz / 2 <= view.loHz + TOL_HZ && centerHz + spanHz / 2 >= view.hiHz - TOL_HZ,
      `pane ${r.id}: the control names ${centerHz} ± ${spanHz / 2}, which does not cover its own ` +
      `viewport ${view.loHz}-${view.hiHz}`);
    named.push(centerHz);
  }
  assert.notEqual(named[0], named[1],
    `two viewports at different windows named the same retune (${named}), so the control is not per-pane`);
});

/** Poll until the backend's coverage map holds something, so the app opens ON the capture. */
async function waitForCoverage(backend, timeoutMs) {
  const t0 = Date.now();
  const q = new URLSearchParams({ f_lo: "1000000", f_hi: "6000000000", cells: "128", rows: "32" });
  for (;;) {
    const cov = await get(backend, `/api/coverage?${q}`).catch(() => null);
    // `GET /api/coverage`'s own shape: `any.cells[]`, each `{state}`. Counted the same way
    // `observedExtent` counts it — "observed", never "not unobserved", because `unknown` is its own
    // state and treating it as coverage is exactly the grey-honesty error this repo keeps refusing.
    const observed = (cov?.any?.cells ?? []).filter((c) => c?.state === "observed").length;
    if (observed > 0) return { observed, ms: Date.now() - t0 };
    if (Date.now() - t0 > timeoutMs) {
      throw new Error(`the mock SDR put nothing in the coverage map in ${timeoutMs} ms: ${JSON.stringify(cov).slice(0, 400)}`);
    }
    await new Promise((r) => setTimeout(r, 500));
  }
}

/** `GET` against a backend, as the app's own client would. */
async function get(backend, path) {
  const r = await fetch(`${backend.origin}${path}`, { headers: { authorization: `Bearer ${backend.token}` } });
  assert.ok(r.ok, `GET ${path} -> ${r.status}`);
  return r.json();
}

/**
 * The viewport window the page itself states, parsed from `.hk-surface-where`.
 *
 * Read off the readout rather than from any internal: the whole point is to tie the retune to the
 * window *the user could see*, and `paneStatuses` formats that string from the very `PaneState` the
 * control planned against.
 */
function viewportOf(where) {
  const m = where.match(/^([\d.]+) MHz ± ([\d.]+) (Hz|kHz|MHz|GHz)/);
  assert.ok(m, `the viewport readout is not a window: ${JSON.stringify(where)}`);
  const mult = { Hz: 1, kHz: 1e3, MHz: 1e6, GHz: 1e9 }[m[3]];
  const centerHz = Number(m[1]) * 1e6, halfHz = Number(m[2]) * mult;
  return { centerHz, halfHz, loHz: centerHz - halfHz, hiHz: centerHz + halfHz };
}
