// T-1003 (MMAP split view): **the status line, the scale bar, Retune and the IQ-backing words live
// INSIDE each pane, per pane — and at two panes nothing is dropped and nothing is clipped.**
//
// The user's shape for split view (2026-09-25, via the supervisor): "A common use would be to look
// at one signal from the past and the current waterfall, and each split needs its own 'Live'
// button." T-1001 gave each pane its Live button; what was still describing "the" viewport was
// everything else — one bottom-left line saying where the hidden active pane looked, one IQ-ring
// sentence saying which side of the ring's horizon THAT pane sat on, and one Retune block under
// Go-to commanding a viewport nothing on screen named. The user's word for that block, later the
// same day: "the info side bar on the bottom left with the Retune button looks out of place."
//
// So each pane carries its own small translucent chip beside its scale bars, and this spec is its
// acceptance, in the real app over the mock SDR, at 1280 x 800 and at 400 px, with TWO panes —
// pane 1 frozen on a past window, pane 2 following the live edge:
//
//   1. **Nothing is dropped**: every pane has a chip, and on it the two bars, the honesty tier, the
//      centre/span/LIVE line, and its IQ-backing words. Each is on screen, non-empty and visible.
//   2. **Nothing is clipped**: each chip's own layout fits inside it (no overflow), and the chip
//      fits inside ITS OWN pane's rectangle — never across the split into the neighbour's picture,
//      never off the viewport.
//   3. **Each pane's IQ words describe its own time position**: the frozen pane must not be told it
//      is "following live", the live one must be, and each pane's backing is the one its own `t1`
//      earns against the ring the same frame reported.
//   4. **The Retune on a chip is that pane's**: it is inside the pane's rectangle and pressable at
//      its own centre, and its words name that pane's destination.
//   5. **Nothing in pane 2 changes when pane 1 is touched** (the set acceptance for every
//      split-view ticket), and reading or arranging any of it reaches no device route.
//
// Unit tier: `ui/test/surface-scale.test.ts` (the chip's composition, its bound, its press and the
// hide-when-nothing-to-say rule), `ui/test/app-map-controls.test.ts` (the retired block).
import test, { after } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser } from "./harness.mjs";
import { startBackend } from "./backend.mjs";
import { paneAct, liveBtn } from "./app-chrome.mjs";

// A free offset inside this lane's range (+0/4/8/20/24/26/28/30 are taken by other specs).
const PORT = Number(process.env.HK_E2E_PORT ?? 8791) + 12;
const SHOTS = process.env.HK_E2E_SHOTS ?? null;
const ART = SHOTS ?? path.join(process.cwd(), "e2e", "artifacts");
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/;

/**
 * Every chip, with the numbers the frame computed it from AND what is actually on screen: each
 * line's rectangle and its `scrollWidth`/`clientWidth` (a line that overflows its own box is
 * clipped, whatever the box's rectangle says), plus the pane rectangle the frame drew it against.
 */
const CHIPS = `JSON.stringify((() => {
  const box = (el) => { const r = el.getBoundingClientRect(); return { x: r.x, y: r.y, w: r.width, h: r.height, r: r.right, b: r.bottom }; };
  const line = (el, cls) => {
    const e = el.querySelector('.' + cls);
    if (!e || e.hidden || e.closest('[hidden]')) return null;
    const r = e.getBoundingClientRect();
    return { text: (e.textContent ?? '').trim(), ...box(e), clipped: e.scrollWidth > e.clientWidth + 1 };
  };
  const ring = document.querySelector('.sf-ring');
  return {
    ring: ring ? { iqS: Number(ring.dataset.iqS), edgeS: Number(ring.dataset.edgeS), backing: ring.dataset.backing } : null,
    chips: [...document.querySelectorAll('.sf-scale')].filter((e) => !e.hidden).map((el) => ({
      pane: el.dataset.pane,
      following: el.dataset.following === 'true',
      backing: el.dataset.backing ?? '',
      t1S: Number(el.dataset.t1Ns) / 1e9,
      paneWPx: Number(el.dataset.paneWPx), paneHPx: Number(el.dataset.paneHPx),
      box: box(el),
      overflow: el.scrollWidth > el.clientWidth + 1 || el.scrollHeight > el.clientHeight + 1,
      freq: line(el, 'sf-scale-row'),
      level: line(el, 'sf-scale-level'),
      where: line(el, 'sf-where'),
      iq: line(el, 'sf-pane-iq'),
      retune: line(el, 'sf-pane-retune-go'),
      why: line(el, 'sf-pane-retune-why'),
    })),
    // The canvas every rectangle above is bounded against; a chip's own paneWPx/paneHPx (the
    // frame's report of the pane it belongs to) is what bounds it to its OWN pane below.
    canvas: box(document.querySelector('.sf-canvas')),
    innerW: innerWidth, innerH: innerHeight,
  };
})())`;

/** Press the hit-tested centre of an element and report what a click there would actually land on. */
const pressable = (sel) => `JSON.stringify((() => { const e = ${sel}; if (!e) return { ok: false, why: 'absent' };
  const r = e.getBoundingClientRect();
  if (r.width < 12 || r.height < 12) return { ok: false, why: 'too small: ' + JSON.stringify(r) };
  const top = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2);
  return { ok: !!top && (top === e || e.contains(top)), why: top ? String(top.className?.baseVal ?? top.className ?? top.tagName) : 'nothing',
    x: r.x + r.width / 2, y: r.y + r.height / 2 }; })())`;

let backendP = null;
const mockBackend = () => (backendP ??= startBackend({ port: PORT, mockDevice: true }));
after(async () => { (await backendP?.catch(() => null))?.stop(); });

for (const [width, height] of [[1280, 800], [400, 800]]) {
  test(`T-1003 at ${width} px: two panes, each with its own status, scale bar, IQ words and Retune — nothing dropped or clipped`, async (t) => {
    const backend = await mockBackend();
    const browser = await Browser.open();
    t.after(() => browser.close());
    const page = await browser.page(undefined, { width, height });
    const shot = async (name) => { await page.frames(2); await page.shot(path.join(ART, `pane-chip-${width}-${name}.png`)); };
    assert.equal(await page.goto(`${backend.origin}/#token=${backend.token}`), "load");
    await page.waitForSurfaceMounted({ timeoutMs: 60000 });
    await page.waitFor("a pane with its chip drawn",
      `!!document.querySelector('.sf-scale')?.dataset.pane && !!document.querySelector('.sf-scale .sf-where')?.textContent`,
      { timeoutMs: 120000 });
    await page.frames(3);
    await shot("1-one-pane");

    // ---- the arrangement: two panes, pane 1 frozen on a past window, pane 2 on the live edge ----
    // Stacked (`split-rows`) at both widths: at 400 px two columns would be 200 px each, which is
    // not the split a phone user makes, and the acceptance is about the chip fitting a real pane.
    await paneAct(page, "split-rows");
    await page.waitFor("two panes", `document.querySelectorAll('.sf-scale:not([hidden])').length === 2`, { timeoutMs: 15000 });
    // Freeze the FIRST pane with its OWN Live button (T-1001) — the press a user makes, and the one
    // that must leave pane 2 alone.
    const before = JSON.parse(await page.eval(CHIPS));
    await page.click(liveBtn(1));
    await page.waitFor("pane 1 to freeze and pane 2 to stay live",
      `(() => { const c = [...document.querySelectorAll('.sf-scale:not([hidden])')];
        return c.length === 2 && c[0].dataset.following === 'false' && c[1].dataset.following === 'true'; })()`,
      { timeoutMs: 15000 });
    // …and let the live edge move on, so the two panes are genuinely at two instants.
    await page.frames(8);
    await shot("2-split");

    const read = JSON.parse(await page.eval(CHIPS));
    t.diagnostic(`at ${width} px: ${JSON.stringify(read.chips.map((c) => ({
      pane: c.pane, following: c.following, backing: c.backing, t1S: c.t1S,
      where: c.where?.text, iq: c.iq?.text, retune: c.retune?.text })))}`);
    assert.equal(read.chips.length, 2, "two panes, and not two chips");
    const [frozen, live] = read.chips;

    // ---- (1) nothing dropped: every pane says all of it, on its own chip ----
    for (const c of read.chips) {
      for (const [name, l] of [["scale bar", c.freq], ["honesty tier", c.level], ["status line", c.where], ["IQ words", c.iq]]) {
        assert.ok(l, `${c.pane}: no ${name} on its chip`);
        assert.ok(l.text.length > 0, `${c.pane}: its ${name} is empty`);
        assert.ok(l.w > 0 && l.h > 0, `${c.pane}: its ${name} is not drawn (${JSON.stringify(l)})`);
      }
      assert.match(c.where.text, /Hz/, `${c.pane}: its status line states no centre/span: ${JSON.stringify(c.where.text)}`);
      assert.match(c.level.text, /detail|overview/, `${c.pane}: its chip states no honesty tier: ${JSON.stringify(c.level.text)}`);
    }

    // ---- (2) nothing clipped: inside its own box, and inside its OWN pane ----
    for (const c of read.chips) {
      assert.equal(c.overflow, false, `${c.pane}: the chip's content overflows the chip (${JSON.stringify(c.box)})`);
      for (const [name, l] of [["status line", c.where], ["IQ words", c.iq], ["honesty tier", c.level]]) {
        assert.equal(l.clipped, false, `${c.pane}: its ${name} is clipped: ${JSON.stringify(l.text)}`);
        assert.ok(l.x >= c.box.x - 1 && l.r <= c.box.r + 1, `${c.pane}: its ${name} runs outside the chip`);
      }
      // The chip is bounded by its pane: its width never exceeds the pane's, and it sits inside the
      // pane's own rectangle — the frame's `paneWPx`/`paneHPx`, against the corner it is anchored to.
      assert.ok(c.box.w <= c.paneWPx + 1, `${c.pane}: the chip is ${c.box.w.toFixed(0)} px wide in a ${c.paneWPx.toFixed(0)} px pane`);
      assert.ok(c.box.h <= c.paneHPx + 1, `${c.pane}: the chip is ${c.box.h.toFixed(0)} px tall in a ${c.paneHPx.toFixed(0)} px pane`);
      assert.ok(c.box.x >= read.canvas.x - 1 && c.box.r <= read.canvas.r + 1 && c.box.y >= read.canvas.y - 1 && c.box.b <= read.canvas.b + 1,
        `${c.pane}: the chip runs off the canvas (${JSON.stringify(c.box)} of ${JSON.stringify(read.canvas)})`);
      assert.ok(c.box.w > 0 && c.box.h > 0, `${c.pane}: the chip is not drawn`);
    }
    // Two panes, two chips, and they do not sit on top of each other.
    const hit = frozen.box.x < live.box.r && frozen.box.r > live.box.x && frozen.box.y < live.box.b && frozen.box.b > live.box.y;
    assert.equal(hit, false, `the two panes' chips overlap: ${JSON.stringify([frozen.box, live.box])}`);

    // ---- (3) each pane's IQ words are about ITS OWN time position ----
    assert.equal(frozen.following, false);
    assert.equal(live.following, true);
    assert.notEqual(frozen.t1S, live.t1S, "the two panes are at the same instant: nothing here measured a per-pane answer");
    assert.equal(live.backing, "live", "the pane following the live edge is not told IQ is being captured");
    assert.match(live.iq.text, /following live/);
    assert.notEqual(frozen.backing, "live", "the FROZEN pane was given the live pane's IQ words");
    assert.doesNotMatch(frozen.iq.text, /following live/,
      `the frozen pane claims it is following live: ${JSON.stringify(frozen.iq.text)}`);
    // …and the word it does carry is the one its own `t1` earns against the ring this frame reported.
    if (read.ring && Number.isFinite(read.ring.iqS)) {
      const want = frozen.t1S >= read.ring.iqS ? "ring" : "outside-ring";
      assert.ok([want, "recording", "unknown"].includes(frozen.backing),
        `the frozen pane is at ${frozen.t1S} against an IQ horizon of ${read.ring.iqS}, and says ${frozen.backing}`);
    }
    // The status lines differ too: one is LIVE, the other a past instant.
    assert.notEqual(frozen.where.text, live.where.text, "two panes at two instants were given one status line");

    // ---- (4) a Retune on a chip is that pane's, and it can be pressed ----
    for (const c of read.chips) {
      if (!c.retune) continue;
      assert.ok(c.retune.x >= c.box.x - 1 && c.retune.r <= c.box.r + 1, `${c.pane}: its Retune is not on its own chip`);
      const p = JSON.parse(await page.eval(pressable(
        `[...document.querySelectorAll('.sf-scale')].find((e) => e.dataset.pane === ${JSON.stringify(c.pane)})?.querySelector('.sf-pane-retune-go')`)));
      assert.equal(p.ok, true, `${c.pane}: its Retune is not pressable at its own centre — a click lands on ${p.why}`);
      assert.ok((c.why?.text ?? "").length > 0, `${c.pane}: its Retune states no destination (the painted-offer rule)`);
    }

    // ---- (5) nothing in pane 2 changed when pane 1 was frozen, and nothing reached the radio ----
    const p2Before = before.chips.length === 2 ? before.chips[1] : null;
    if (p2Before) {
      assert.equal(live.following, p2Before.following, "freezing pane 1 changed whether pane 2 follows the live edge");
      assert.equal(live.pane, p2Before.pane, "freezing pane 1 renamed pane 2");
    }
    assert.deepEqual(page.requests.filter((r) => CONTROL.test(r.url)).map((r) => r.url), [],
      "splitting, freezing or reading a chip reached a device route");
    assert.deepEqual(page.exceptions, [], "uncaught exception");
  });
}
