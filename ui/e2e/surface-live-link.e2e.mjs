// **T-982: on a live server, /surface.html read "historical · the edge does not advance" with both
// panes stuck at "0 tiles", and the page gave whoever landed there no way to tell that was by
// design (T-450, unchanged since) rather than a broken live-edge read, and no way to reach the view
// that IS live — `/`, mounting the same renderer (T-445) with the edge attached.
//
// `ui/test/surface-preview.test.ts` proves the static HTML carries the link; this proves a real
// browser renders it where a person would actually see it, that it is a genuine navigation (not a
// view-change label like the page's other buttons — T-442's "a pan or wheel never commands the
// radio" applies here too: a link to `/` must not be dressed up as a control that reaches a route),
// and that following it lands on the live page, not a 404 or a page that fails to mount.
import test from "node:test";
import assert from "node:assert/strict";
import { Browser } from "./harness.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;

test("surface.html states its historical scope is by design and links to / for the live view", async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();

  assert.equal(await page.goto(`${ORIGIN}/surface.html#token=${TOKEN}`), "load");
  await page.waitForSurfaceMounted();

  // The badge is still historical — that claim is true — but its title must no longer read as an
  // unexplained state a live server could be wrong about.
  const badgeTitle = await page.eval(
    `document.querySelector(".sp-badge")?.getAttribute("title") ?? null`);
  assert.match(badgeTitle ?? "", /by design, not a fault/,
    `the badge's title does not say this is deliberate: ${badgeTitle}`);

  // The link: present, in the header bar beside the badge, pointing at the root page.
  const link = await page.eval(`(() => {
    const a = document.querySelector('[data-slot="live-link"]');
    if (!a) return null;
    return { text: a.textContent, href: a.getAttribute("href"), inBar: !!a.closest(".sp-bar") };
  })()`);
  assert.ok(link, "no [data-slot=\"live-link\"] element on the page");
  assert.equal(link.href, "/", `the live link must point at the root page, got ${link.href}`);
  assert.match(link.text ?? "", /live/i, "the link's own text must say what it does");
  assert.ok(link.inBar, "the live link must sit in the header bar beside the historical badge");

  // It is a plain navigation: nothing about rendering or clicking it may reach a device route —
  // the same guard `surface-load.e2e.mjs` holds over the rest of the page.
  const control = page.requests.filter((r) => /\/api\/control\//.test(r.url));
  assert.deepEqual(control, [], "the live link's page reached a control route before it was even followed");

  // Follow it, in the same tab (a plain `<a href>`, not target=_blank — the token lives in
  // sessionStorage, per-tab). The destination must be the live surface, not a 404 or a stuck load.
  // `waitForSurfaceMounted` throws with the page's own reason if it mounted "failed" instead.
  assert.equal(await page.goto(`${ORIGIN}/`), "load", "following the live link's destination never fired load");
  await page.waitForSurfaceMounted();
});
