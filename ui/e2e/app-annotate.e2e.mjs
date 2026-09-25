// T-984 (MAP-20/MAP-21): authoring an annotation, driven through the real UI gesture, had no e2e
// coverage at all — only the pure functions (`ui/test/surface-annotations.test.ts`,
// `ui/test/app-explore-annotate.test.ts`) and the panel's own model (`ui/test/app-research.test.ts`)
// were exercised. Two defects this tier alone can see (T-820's worker, task-t820 624fbe16):
//
//  (1) with the `research` overlay layer switched on, an annotation drew TWICE — once as T-820's
//      always-on dashed rose box (`annotationQuads`), and again as T-821's "research-box" mark
//      (`researchBoxesFor`) — because two independent renderers both owned the same object.
//  (2) a just-saved annotation reached the Research panel only on that panel's own 15 s poll
//      (`RESEARCH_REFRESH_MS`), even when the panel was already open and watching.
//
// Neither is visible to a unit test: (1) is glue code inside `centre/surface.ts`'s one `marks`
// closure, never a pure function with its own test file, and (2) is a *timing* property of two
// independently-polling parts of the running app. This drives the real gesture (Annotate button,
// drag a box; Pin button, tap a point) through a real browser and asserts on what the page itself
// now states it drew (`.sf-stage[data-annotation-draws]`, T-984 — the same "state it, don't
// re-derive it" idiom `data-overlay-layers` already uses) and on the Research table's own DOM.
import test from "node:test";
import assert from "node:assert/strict";
import { Browser } from "./harness.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;

/** Wait for the surface to draw and the floating cluster (Annotate/Pin/Layers/Research) to mount. */
async function ready(page) {
  await page.waitForSurfaceMounted({ timeoutMs: 60000 });
  await page.waitFor("the surface to draw and the annotate/pin/research controls to mount",
    `!!document.querySelector('.sf-canvas') && document.querySelector('.sf-canvas').width > 200 &&
     !!document.querySelector('.map-annotate-btn') && !!document.querySelector('.map-pin-btn') &&
     !!document.querySelector('.map-research-btn')`, { timeoutMs: 60000 });
}

/** `.sf-stage[data-annotation-draws]`: `[{id, label, dashed, researchBox}, …]` for every annotation
 * currently loaded on the active pane (T-984). `researchBox` must always be 0 — an annotation's
 * geometry reaching `researchBoxesFor` a second time is exactly the (1) defect. */
const DRAWS = "document.querySelector('.sf-stage')?.dataset.annotationDraws ?? '[]'";

/** Every GET this page has made to the windowed annotations read (the Research panel's own poll and
 * initial load use it; a POST to the same path is a different method and not counted here). */
function annotationGets(page) {
  return page.requests.filter((r) => r.method === "GET" && r.url.startsWith("/api/annotations?"));
}

test("T-984: an annotation box is drawn once, whichever layers are on, and reaches the Research panel from the create response", async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await ready(page);

  // Open the Research panel FIRST and let its initial load settle — the panel is already open and
  // watching when the annotation is authored, which is the scenario (2) is about (a panel opened
  // AFTER the annotation exists would just load it fresh, proving nothing about the create-response
  // path). "Bookmarks" is the server's reserved collection, always present once loaded.
  await page.click("document.querySelector('.map-research-btn')");
  await page.waitFor("the Research panel to open and load its reserved Bookmarks collection",
    `!document.querySelector('.research')?.hidden &&
     [...document.querySelectorAll('.research-colls li')].some((li) => li.dataset.collection === '00000000-0000-7000-8000-000000000b00')`,
    { timeoutMs: 30000 });

  const label = `T-984 box ${Date.now()}`;
  page.dialogText = label;
  await page.click("document.querySelector('.map-annotate-btn')");
  await page.waitFor("annotate mode to arm", `document.querySelector('.map-annotate-btn').getAttribute('aria-pressed') === 'true'`,
    { timeoutMs: 5000 });

  const rect = await page.$rect(".sf-canvas");
  const getsBeforeDraw = annotationGets(page).length;
  await page.drag(
    { x: rect.x + rect.w * 0.3, y: rect.y + rect.h * 0.3 },
    { x: rect.x + rect.w * 0.6, y: rect.y + rect.h * 0.65 });
  assert.equal(page.dialogs.at(-1)?.type, "prompt", `no label prompt was opened: ${JSON.stringify(page.dialogs)}`);

  // The write landed.
  let posted;
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    posted = page.requests.find((r) => r.method === "POST" && r.url.endsWith("/api/annotations") && r.status !== null);
    if (posted) break;
    await page.frames(1);
  }
  assert.ok(posted, "the box never reached POST /api/annotations");
  assert.ok(posted.status >= 200 && posted.status < 300, `POST /api/annotations answered ${posted.status}`);

  // (2) THE CLAIM: the row is in the Research table before any FURTHER windowed GET to
  // /api/annotations happens. The panel already did its one GET when it opened, above (before this
  // annotation existed); if a second one had to land before the row showed, that second GET is
  // counted here and the assertion fails — proving the row came from the poll, not the create
  // response.
  await page.waitFor(`the "${label}" row to appear in the Research table`,
    `[...document.querySelectorAll('.research-name')].some((e) => e.textContent.includes(${JSON.stringify(label)}))`,
    { timeoutMs: 8000 });
  const getsAfterRow = annotationGets(page).length;
  t.diagnostic(`GET /api/annotations before draw: ${getsBeforeDraw}, once the row showed: ${getsAfterRow}`);
  assert.equal(getsAfterRow, getsBeforeDraw,
    "a new GET /api/annotations happened before the row appeared — it came from the panel's poll, not the create response");

  // (1) THE CLAIM: the canvas's own draw statement says this annotation is drawn by exactly one
  // path (`dashed`), never also as a `research-box`, with the `research` overlay layer OFF (its
  // default) — and, the actual reported scenario, still exactly once once that layer is switched ON.
  // The Research row updates the moment the store does (T-984's fix), but `.sf-stage`'s own draw
  // statement is only refreshed on the render loop's next frame for the active pane — give it a few
  // before reading it, so this does not race the render it is asserting on.
  await page.frames(3);
  const before = JSON.parse(await page.eval(DRAWS)).find((a) => a.label === label);
  assert.ok(before, `the new annotation is not in the page's own draw statement: ${await page.eval(DRAWS)}`);
  assert.deepEqual([before.dashed, before.researchBox], [1, 0], "drawn more than once with the research layer off");

  await page.click("document.querySelector('.map-layers-btn')");
  await page.waitFor("the layers menu to open", `!document.querySelector('#map-layers').hidden`, { timeoutMs: 5000 });
  await page.click(`document.querySelector('#map-layers input[data-layer="research"]')`);
  await page.waitFor("the research layer to switch on", `document.querySelector('#map-layers input[data-layer="research"]').checked`,
    { timeoutMs: 5000 });
  await page.frames(2);
  const withResearchOn = JSON.parse(await page.eval(DRAWS)).find((a) => a.label === label);
  assert.ok(withResearchOn, "the annotation dropped out of the draw statement once the research layer was switched on");
  assert.deepEqual([withResearchOn.dashed, withResearchOn.researchBox], [1, 0],
    "T-820's finding (1): the research layer being on drew the annotation a second time, as a research-box");

  const control = page.requests.filter((r) => /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/.test(r.url));
  assert.deepEqual(control.map((r) => r.url), [], "authoring an annotation reached a device route");
  assert.deepEqual(page.exceptions, [], "uncaught exception while authoring the annotation");
});

test("T-984: a pin (marker annotation) is drawn once and reaches the Research panel from the create response", async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await ready(page);

  await page.click("document.querySelector('.map-research-btn')");
  await page.waitFor("the Research panel to open and load its reserved Bookmarks collection",
    `!document.querySelector('.research')?.hidden &&
     [...document.querySelectorAll('.research-colls li')].some((li) => li.dataset.collection === '00000000-0000-7000-8000-000000000b00')`,
    { timeoutMs: 30000 });

  const label = `T-984 pin ${Date.now()}`;
  page.dialogText = label;
  await page.click("document.querySelector('.map-pin-btn')");
  await page.waitFor("pin mode to arm", `document.querySelector('.map-pin-btn').getAttribute('aria-pressed') === 'true'`,
    { timeoutMs: 5000 });

  const rect = await page.$rect(".sf-canvas");
  const getsBeforeTap = annotationGets(page).length;
  const at = { x: rect.x + rect.w * 0.5, y: rect.y + rect.h * 0.2 };
  // A tap: press and release at the same point, well under the drag threshold (`DRAG_PX`), which is
  // what `onAnnotatePoint` requires to read a click as a pin drop rather than a pan.
  await page.mouse("mousePressed", at.x, at.y, { buttons: 1, clickCount: 1 });
  await page.mouse("mouseReleased", at.x, at.y, { buttons: 0, clickCount: 1 });
  assert.equal(page.dialogs.at(-1)?.type, "prompt", `no label prompt was opened: ${JSON.stringify(page.dialogs)}`);

  let posted;
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    posted = page.requests.find((r) => r.method === "POST" && r.url.endsWith("/api/annotations") && r.status !== null);
    if (posted) break;
    await page.frames(1);
  }
  assert.ok(posted, "the pin never reached POST /api/annotations");
  assert.ok(posted.status >= 200 && posted.status < 300, `POST /api/annotations answered ${posted.status}`);

  await page.waitFor(`the "${label}" row to appear in the Research table`,
    `[...document.querySelectorAll('.research-name')].some((e) => e.textContent.includes(${JSON.stringify(label)}))`,
    { timeoutMs: 8000 });
  const getsAfterRow = annotationGets(page).length;
  t.diagnostic(`GET /api/annotations before tap: ${getsBeforeTap}, once the row showed: ${getsAfterRow}`);
  assert.equal(getsAfterRow, getsBeforeTap,
    "a new GET /api/annotations happened before the row appeared — it came from the panel's poll, not the create response");

  // The Research row updates the moment the store does (T-984's fix), but `.sf-stage`'s own draw
  // statement is only refreshed on the render loop's next frame for the active pane — give it a
  // few before reading it, so this does not race the render it is asserting on.
  await page.frames(3);
  const drawn = JSON.parse(await page.eval(DRAWS)).find((a) => a.label === label);
  assert.ok(drawn, `the new pin is not in the page's own draw statement: ${await page.eval(DRAWS)}`);
  assert.deepEqual([drawn.dashed, drawn.researchBox], [1, 0], "the pin is drawn more than once");

  const control = page.requests.filter((r) => /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/.test(r.url));
  assert.deepEqual(control.map((r) => r.url), [], "authoring a pin reached a device route");
  assert.deepEqual(page.exceptions, [], "uncaught exception while authoring the pin");
});
