// T-955: reload restores the previous view + a stale retune offer; follow-live froze instead of
// following, after an API retune.
//
// Found live 2026-09-25 (the explorer, real HackRF, 162.2 -> 144.6 MHz): a reloaded page restored
// the PREVIOUS tuned view and painted a stale "Retune to 162.2000 MHz" offer, instead of opening on
// the window the front end is NOW tuned to (the T-376 bootstrap rule, docs/16 §8). A second run
// opened on "100–1100 MHz x 1.6 h" with the actually-tuned band drawn as a sliver inside it — the
// same defect: `openingWindow` boxed the union of every tuning a session had ever held, not the one
// it holds now.
//
// This drives the retune through the control API directly (never a page gesture — CLAUDE.md: e2e
// goes through the device interface, and a retune here is deliberately NOT the thing under test),
// reloads, and asserts what the PAGE draws: the pane's window, and the retune offer beside it.
//
// ——— RED ON CODE BEFORE T-955 ———
// Revert `recentObservedExtent`'s use in `probeSurface` (ui/src/surface/preview.ts) back to plain
// `observedExtent` and this fails: the reloaded pane's centre sits near the OLD band (or between the
// two), not within tolerance of the retuned one.
import test from "node:test";
import assert from "node:assert/strict";
import { Browser } from "./harness.mjs";
import { startBackend } from "./backend.mjs";

const PORT = Number(process.env.HK_E2E_PORT ?? 8791) + 28;
const AWAY_HZ = 433.92e6; // outside fm_100p8M_2p4M's 2.4 MHz recording — the mock's synthesised floor

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** `GET`, riding out the route's backpressure (`503` is "ask again", never "no" — T-690). */
async function get(backend, p, { tries = 60, waitMs = 200 } = {}) {
  for (let i = 0; ; i++) {
    const r = await fetch(`${backend.origin}${p}`, { headers: { authorization: `Bearer ${backend.token}` } });
    if (r.ok) return r.json();
    assert.ok(r.status === 503 && i < tries, `GET ${p} -> ${r.status}`);
    await sleep(waitMs);
  }
}

/** The one gated device action, through the API directly — never a page gesture (this is not what
 * is under test). Keeps the span in force; retries `device_busy`/backpressure. */
async function retune(backend, centerHz) {
  const nav = await get(backend, "/api/navigation");
  const w = nav.windows?.[0];
  assert.ok(w, `the mock reported no capture window: ${JSON.stringify(nav.windows)}`);
  for (let i = 0; i < 20; i++) {
    const r = await fetch(`${backend.origin}/api/control/window`, {
      method: "POST",
      headers: { authorization: `Bearer ${backend.token}`, "content-type": "application/json" },
      body: JSON.stringify({ center_hz: centerHz, sample_rate_hz: w.span_hz }),
    });
    if (r.ok) return;
    assert.ok(r.status === 409 || r.status === 503, `retune -> ${r.status}: ${await r.text()}`);
    await sleep(250);
  }
  assert.fail("the mock never accepted the retune");
}

/** The pane's frequency window, parsed from its scale block's `data-where` (T-478: numbers, never
 * the string; T-996: the per-viewport panel that used to carry the sentence is retired). */
function windowOf(where) {
  const m = /^([\d.]+) MHz ± ([\d.]+) (Hz|kHz|MHz|GHz)/.exec(where ?? "");
  assert.ok(m, `the pane readout is not a frequency window: ${JSON.stringify(where)}`);
  const mult = { Hz: 1, kHz: 1e3, MHz: 1e6, GHz: 1e9 }[m[3]];
  const centerHz = Number(m[1]) * 1e6, halfHz = Number(m[2]) * mult;
  return { centerHz, spanHz: 2 * halfHz };
}

async function pane0(page) {
  const ROWS = `JSON.stringify([...document.querySelectorAll('.sf-scale')].map((v) => ({
    viewport: 'pane',
    following: v.dataset.following === 'true',
    where: v.dataset.where ?? '',
    why: document.querySelector('.map-retune-why')?.textContent ?? '',
  })))`;
  const rows = JSON.parse(await page.eval(ROWS));
  const r = rows.filter((x) => x.viewport === "pane")[0];
  assert.ok(r, "the page is drawing no pane at all");
  return r;
}

async function openApp(backend) {
  const browser = await Browser.open();
  const page = await browser.page();
  assert.equal(await page.goto(`${backend.origin}/#token=${backend.token}`), "load");
  await page.waitForSurfaceMounted({ timeoutMs: 30000 });
  await page.waitFor("a pane readout to exist",
    "!!document.querySelector('.sf-scale')?.dataset.where",
    { timeoutMs: 20000 });
  await page.frames(4);
  return { browser, page };
}

test("T-955: reload after an API retune opens on the NEW tuned window, with no stale retune offer", async (t) => {
  const backend = await startBackend({ port: PORT, mockDevice: true });
  t.after(() => backend.stop());
  let browser;
  try {
    const first = await openApp(backend);
    const before = await pane0(first.page);
    t.diagnostic(`before retune: ${before.where}`);
    first.browser.close();

    // The retune the user did: an explicit device action, through the API, never a page gesture.
    await retune(backend, AWAY_HZ);
    for (let i = 0; i < 40; i++) {
      const nav = await get(backend, "/api/navigation");
      if (Math.abs((nav.windows?.[0]?.center_hz ?? 0) - AWAY_HZ) < 100) break;
      await sleep(200);
    }
    const tunedNav = await get(backend, "/api/navigation");
    const tuned = tunedNav.windows?.[0];
    assert.ok(tuned && Math.abs(tuned.center_hz - AWAY_HZ) < 100,
      `the mock never reached the retuned centre: ${JSON.stringify(tunedNav.windows)}`);

    // Reload — a fresh page load, exactly like the user's F5. Nothing here is a gesture.
    const { browser: b2, page } = await openApp(backend);
    browser = b2;
    t.after(() => browser.close());

    const after = await pane0(page);
    t.diagnostic(`after reload: ${after.where} · following ${after.following} · offer: ${after.why}`);
    const w = windowOf(after.where);
    assert.ok(Math.abs(w.centerHz - tuned.center_hz) < tuned.span_hz,
      `the reloaded pane opened at ${(w.centerHz / 1e6).toFixed(4)} MHz, not near the retuned ` +
      `${(tuned.center_hz / 1e6).toFixed(4)} MHz — a stale view, or a sliver of a union box`);
    assert.ok(w.spanHz <= tuned.span_hz * 4,
      `the reloaded pane's span (${(w.spanHz / 1e6).toFixed(3)} MHz) is many times the tuned window's ` +
      `(${(tuned.span_hz / 1e6).toFixed(3)} MHz): the sliver-in-a-wide-box shape of the bug`);

    // No stale offer: the retune control beside the pane must not still be naming the OLD band.
    assert.ok(!after.why.includes("100.8000"),
      `the retune offer still names the band the radio LEFT: ${JSON.stringify(after.why)}`);
  } finally {
    browser?.close();
  }
});
