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
// ——— RED ON CODE BEFORE THE T-955 FIX ROUND ———
// On 448a9a65 the FAB press froze the drifted-but-following pane (it read `isFollowing`, never the
// tuned window), the painted Go-to offer outlived the retune, and the reload 30 s after a retune
// opened on the union of both tunings (recency counted from the grid's end, not the newest row).
//
// Screenshots land in $HK_E2E_SHOTS (when set) for the hand-back.
import test from "node:test";
import assert from "node:assert/strict";
import { Browser } from "./harness.mjs";
import { startBackend } from "./backend.mjs";

const PORT = Number(process.env.HK_E2E_PORT ?? 8791) + 28;
const AWAY_HZ = 433.92e6; // outside fm_100p8M_2p4M's 2.4 MHz recording — the mock's synthesised floor

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const SHOTS = process.env.HK_E2E_SHOTS ?? null;
const shot = (page, name) => (SHOTS ? page.shot(`${SHOTS}/${name}.png`) : Promise.resolve());
/** The page's own view of the tuned window, from the navigation poll it already runs. */
const FAB = "document.querySelector('.map-ctl .map-fab')";
const OFFER_SHOWN = "!document.querySelector('.map-ctl .map-offer').hidden";
const OFFER_TEXT = "document.querySelector('.map-ctl .map-offer-why').textContent";
/** Go-to, the way the user did it: type into the box and submit (a view move, never a retune). */
async function goTo(page, text) {
  await page.eval(`(() => { const i = document.querySelector('.map-ctl .map-goto input'); i.value = ${JSON.stringify(text)};
    i.closest('form').requestSubmit(); })()`);
  await page.frames(3);
}

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

async function waitTuned(backend, hz) {
  for (let i = 0; i < 60; i++) {
    const nav = await get(backend, "/api/navigation");
    const w = nav.windows?.[0];
    if (w && Math.abs(w.center_hz - hz) < 100) return w;
    await sleep(200);
  }
  assert.fail(`the mock never reached ${hz} Hz`);
}

test("T-955: after an API retune — the Go-to offer does not outlive it, follow-live brings a drifted pane to the tuned live edge, and a reload 30 s later opens on the NEW window", async (t) => {
  const backend = await startBackend({ port: PORT, mockDevice: true });
  t.after(() => backend.stop());
  let browser;
  try {
    const first = await openApp(backend);
    browser = first.browser;
    const page = first.page;
    const before = await pane0(page);
    const tuned0 = await waitTuned(backend, (await get(backend, "/api/navigation")).windows[0].center_hz);
    t.diagnostic(`before retune: ${before.where} (tuned ${tuned0.center_hz})`);

    // (2) The explorer's 04:16 step: Go-to a band the radio is not on — the offer is painted.
    await goTo(page, "433.92M");
    await page.waitFor("the Go-to retune offer to be painted", OFFER_SHOWN, { timeoutMs: 5000 });
    t.diagnostic(`offer: ${await page.eval(OFFER_TEXT)}`);
    // …then the radio is retuned THERE through the API (not the offer): the offer is now stale.
    await retune(backend, AWAY_HZ);
    const tuned = await waitTuned(backend, AWAY_HZ);
    const retunedAt = Date.now();
    await page.waitFor("the stale Go-to offer to be withdrawn once the radio holds that window",
      `!(${OFFER_SHOWN})`, { timeoutMs: 15000 });

    // (1) The explorer's 0428 pane: FOLLOWING in time, but at a frequency the radio has left.
    await goTo(page, `${tuned0.center_hz / 1e6}M`);
    const drifted = await pane0(page);
    assert.equal(drifted.following, true, "precondition: the drifted pane still follows live time");
    assert.ok(Math.abs(windowOf(drifted.where).centerHz - tuned0.center_hz) < 1e6, `precondition: ${drifted.where}`);
    await page.waitFor("the FAB to say the pane is off the tuned window", `${FAB}.classList.contains('off-tuned')`, { timeoutMs: 15000 });
    await shot(page, "t955-follow-before");
    await page.click(FAB);
    await page.frames(4);
    const followed = await pane0(page);
    assert.equal(await page.eval(OFFER_SHOWN), false, "the Go-to offer for the band the pane LEFT survived the follow-live press");
    await sleep(2000);
    await shot(page, "t955-follow-after");
    t.diagnostic(`follow-live from a drifted following pane: ${drifted.where} -> ${followed.where}`);
    assert.equal(followed.following, true, `the follow-live press FROZE the pane (explorer 0430): ${followed.where}`);
    assert.ok(Math.abs(windowOf(followed.where).centerHz - tuned.center_hz) < tuned.span_hz,
      `follow-live left the pane at ${followed.where}, not the tuned ${tuned.center_hz / 1e6} MHz`);
    first.browser.close();
    browser = null;

    // (3) The reported flow: a genuine reload 30 s after the retune.
    const wait = 30_000 - (Date.now() - retunedAt);
    if (wait > 0) await sleep(wait);
    const second = await openApp(backend);
    browser = second.browser;
    const after = await pane0(second.page);
    await sleep(3000);
    await shot(second.page, "t955-reload-30s");
    t.diagnostic(`after reload ${Math.round((Date.now() - retunedAt) / 1000)} s after the retune: ${after.where} · following ${after.following} · offer: ${after.why}`);
    const w = windowOf(after.where);
    assert.ok(Math.abs(w.centerHz - tuned.center_hz) < tuned.span_hz,
      `the reloaded pane opened at ${(w.centerHz / 1e6).toFixed(4)} MHz, not near the retuned ` +
      `${(tuned.center_hz / 1e6).toFixed(4)} MHz — a stale view, or a sliver of a union box`);
    assert.ok(w.spanHz <= tuned.span_hz * 4,
      `the reloaded pane's span (${(w.spanHz / 1e6).toFixed(3)} MHz) is many times the tuned window's ` +
      `(${(tuned.span_hz / 1e6).toFixed(3)} MHz): the sliver-in-a-wide-box shape of the bug`);
    assert.equal(await second.page.eval(OFFER_SHOWN), false, "a freshly loaded page painted a Go-to offer");
  } finally {
    browser?.close();
  }
});
