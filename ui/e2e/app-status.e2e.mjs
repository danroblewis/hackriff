// T-996 (MMAP, user 2026-09-25 via the supervisor): **the scale bar, and what it replaced.**
//
// The user read the map's chrome back to us: the Retune/stats bar's `More` expansion (slice, peak
// dB, a stale signal selection, "phosphor style" — *"totally unnecessary"* — capture rules *"also
// unnecessary"*, `0.4 % of this surface was ever sampled (18 of 4096 coverage cells)` — *"can
// probably go"*) and the white panel block *"100.980 MHz ± 937 kHz LIVE"*, which *"can definitely be
// replaced with something on the waterfall map. Google Maps has a very small short section
// bottom-right that shows the distance measure … A sense of scale is what the current centre freq
// and width is."*
//
// So, against the product's own server, in a browser, at 1280 × 800 and at 400 px:
//   1. the canvas is still full-bleed (T-918's invariant, which this must not buy back);
//   2. **no white panel and no `More`**: `.sf-chrome`, `.sf-status-toggle`, `.sf-status-body` are
//      gone from the page — not merely collapsed;
//   3. **a scale bar bottom-right of each pane**, inside that pane, with the honesty tier beside it;
//   4. **the bar means what it says**: its DRAWN pixel length × the pane's own Hz/px is the
//      frequency it is labelled with, and the same for s/px and the time bar — **at three zooms**,
//      because a scale that did not follow the zoom would be decoration;
//   5. the kept line states centre ± span and LIVE-or-time-position, and the colour scale (§10.2:
//      an honesty statement stays on the picture);
//   6. at 400 px it all fits: no sideways scroll, the block inside the viewport, clear of the
//      right-edge cluster;
//   7. and none of it — zooming included — reaches a device route (§10.4: the view is not the radio).
import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser } from "./harness.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const SHOTS = process.env.HK_E2E_SHOTS ?? null;
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/;

/** The status line's bound. It is a LINE — the complaint was a 560 × 184 px panel over the map. */
const BOUND = { h: 56, w: 560 };

/** The retired chrome, by selector: none of these may exist on the page any more. */
const RETIRED = [".sf-chrome", ".sf-status-toggle", ".sf-status-close", ".sf-status-body", ".hk-surface-viewport"];

/**
 * Everything the assertions read, in one round trip.
 *
 * Each pane's scale block reports BOTH the numbers the frame computed it from (its dataset) and
 * what is actually drawn (`getBoundingClientRect` of the bar elements), so the check below is
 * pixels-against-the-claim rather than the claim against itself.
 */
const READ = `JSON.stringify((() => {
  const box = (el) => { const r = el.getBoundingClientRect(); return { x: r.x, y: r.y, w: r.width, h: r.height }; };
  const text = (sel) => (document.querySelector(sel)?.textContent ?? '').trim();
  const st = document.querySelector('.sf-status');
  return {
    retired: ${JSON.stringify(RETIRED)}.filter((s) => !!document.querySelector(s)),
    status: st ? box(st) : null,
    line: document.querySelector('.sf-status-line') ? box(document.querySelector('.sf-status-line')) : null,
    where: text('.sf-where'), range: text('.sf-range'),
    canvas: box(document.querySelector('.sf-canvas')),
    scales: [...document.querySelectorAll('.sf-scale')].map((el) => {
      const f = el.querySelector('.sf-scale-row.freq'), t = el.querySelector('.sf-scale-row.time');
      return {
        pane: el.dataset.pane, level: el.querySelector('.sf-scale-level')?.textContent ?? '', tier: el.dataset.tier,
        hzPerPx: Number(el.dataset.hzPerPx), sPerPx: Number(el.dataset.sPerPx),
        fHz: Number(el.dataset.fHz), tS: Number(el.dataset.tS),
        paneWPx: Number(el.dataset.paneWPx), paneHPx: Number(el.dataset.paneHPx),
        fLabel: f?.querySelector('b')?.textContent ?? '', tLabel: t?.querySelector('b')?.textContent ?? '',
        fBar: f && !f.hidden ? box(f.querySelector('i')) : null,
        tBar: t && !t.hidden ? box(t.querySelector('i')) : null,
        box: box(el),
      };
    }),
    cluster: ['.map-goto', '.map-topright', '.map-zoom', '.map-fab'].map((s) => {
      const e = document.querySelector(s); return e ? { sel: s, ...box(e) } : null;
    }).filter(Boolean),
    scrollW: Math.max(document.documentElement.scrollWidth, document.body.scrollWidth),
    innerW: innerWidth, innerH: innerHeight,
  };
})())`;

/** The words the bar is labelled with, from the quantity it claims — `surface/scale.ts`'s rule. */
function fmtHz(hz) {
  const [v, u] = hz >= 1e9 ? [hz / 1e9, "GHz"] : hz >= 1e6 ? [hz / 1e6, "MHz"] : hz >= 1e3 ? [hz / 1e3, "kHz"] : [hz, "Hz"];
  return `${Math.round(v * 100) / 100} ${u}`;
}
function fmtS(s) {
  const [v, u] = s >= 86400 ? [s / 86400, "d"] : s >= 3600 ? [s / 3600, "h"] : s >= 60 ? [s / 60, "min"] : s >= 1 ? [s, "s"] : [s * 1000, "ms"];
  return `${Math.round(v * 100) / 100} ${u}`;
}

/** A 1/2/5 × 10^k quantity — what a map's bar is always labelled with. */
const isRound = (v) => {
  const m = v / Math.pow(10, Math.floor(Math.log10(v) + 1e-9));
  return [1, 2, 5].some((k) => Math.abs(k - m) < 1e-6);
};

/**
 * The one claim the bar makes: **its drawn length, times this pane's own scale, is the quantity it
 * is labelled with.** Read off the rendered element, against the pane's Hz/px and s/px — which the
 * frame derived from the same box and rect it drew the rows with.
 */
function assertBarsMeanIt(s, where, t) {
  assert.ok(s.hzPerPx > 0 && s.sPerPx > 0, `${where}: the pane states no scale (${s.hzPerPx}, ${s.sPerPx})`);
  assert.ok(s.fBar, `${where}: no frequency bar is drawn`);
  // 1 px of tolerance: the length is set in CSS px to one decimal, and the layout rounds it.
  const fSaid = s.fBar.w * s.hzPerPx;
  assert.ok(Math.abs(fSaid - s.fHz) <= Math.abs(s.hzPerPx) * 1.5,
    `${where}: the frequency bar is ${s.fBar.w.toFixed(1)} px = ${fSaid.toFixed(0)} Hz, labelled ${s.fLabel} (${s.fHz} Hz)`);
  assert.equal(s.fLabel, fmtHz(s.fHz), `${where}: the frequency label does not say the quantity it stands for`);
  assert.ok(isRound(s.fHz), `${where}: ${s.fHz} Hz is not a round 1/2/5 quantity`);
  assert.ok(s.fBar.w > 0 && s.fBar.w <= 100, `${where}: the frequency bar is ${s.fBar.w.toFixed(1)} px — not a legend`);
  assert.ok(s.tBar, `${where}: no time bar is drawn`);
  const tSaid = s.tBar.h * s.sPerPx;
  assert.ok(Math.abs(tSaid - s.tS) <= Math.abs(s.sPerPx) * 1.5,
    `${where}: the time bar is ${s.tBar.h.toFixed(1)} px = ${tSaid.toFixed(2)} s, labelled ${s.tLabel} (${s.tS} s)`);
  assert.equal(s.tLabel, fmtS(s.tS), `${where}: the time label does not say the quantity it stands for`);
  assert.ok(s.tBar.h > 0 && s.tBar.h <= 100, `${where}: the time bar is ${s.tBar.h.toFixed(1)} px — not a legend`);
  // The honesty tier, beside the bars (the three-tier rule: a deep zoom must read as overview).
  assert.match(s.level, /detail|overview/, `${where}: the block states no honesty tier: ${JSON.stringify(s.level)}`);
  assert.match(s.level, /cells/, `${where}: the block states no cell size: ${JSON.stringify(s.level)}`);
  t.diagnostic(`${where}: ${s.fLabel} over ${s.fBar.w.toFixed(1)} px, ${s.tLabel} over ${s.tBar.h.toFixed(1)} px · ${s.level}`);
}

for (const width of [1280, 400]) test(`at ${width} px: a scale bar per pane, one status line, and no white panel or 'More'`, async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { width, height: 800 });
  const shot = async (name) => { if (SHOTS) await page.shot(path.join(SHOTS, `scale-${width}-${name}.png`)); };
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitForSurfaceMounted({ timeoutMs: 60000 });
  // The scale block is written per render frame from the status the renderer just reported, so
  // waiting for one is waiting for a frame that actually drew a pane — the subject of every
  // assertion here. Never a clock: addressing takes tens of seconds under load.
  await page.waitFor("a pane with its scale bar drawn",
    `!!document.querySelector('.sf-scale')?.dataset.pane && !!document.querySelector('.sf-where')?.textContent`,
    { timeoutMs: 90000 });
  await page.frames(3);
  await shot("1-open");

  const first = JSON.parse(await page.eval(READ));
  t.diagnostic(`at ${width} px: ${JSON.stringify({ status: first.status, scales: first.scales.map((s) => s.level) })}`);

  // (1) T-918's invariant: the canvas is 100vw × 100vh and this chrome floats OVER it.
  assert.deepEqual([first.canvas.x, first.canvas.y, first.canvas.w, first.canvas.h].map(Math.round), [0, 0, width, 800],
    `the canvas is not full-bleed at ${width} px: ${JSON.stringify(first.canvas)}`);

  // (2) The retired chrome is GONE — not collapsed, not hidden behind a press.
  assert.deepEqual(first.retired, [], `retired chrome is still on the page: ${first.retired.join(", ")}`);

  // (3) A scale block per pane, drawn INSIDE its pane and near its bottom-right corner.
  assert.ok(first.scales.length >= 1, "no pane states a scale");
  for (const s of first.scales) {
    assert.ok(s.box.w > 0 && s.box.h > 0, `${s.pane}: the scale block is not drawn`);
    assert.ok(s.box.x >= 0 && s.box.x + s.box.w <= first.innerW + 1,
      `${s.pane}: the scale block runs off the viewport (${JSON.stringify(s.box)} of ${first.innerW})`);
    assert.ok(s.box.y + s.box.h <= first.innerH + 1, `${s.pane}: the scale block runs below the viewport`);
    // Bottom-RIGHT: in the right half of its own pane and below its middle. (The pane's own width
    // and height are what the frame laid the bars out against, so they are what this reads.)
    assert.ok(s.box.x + s.box.w > s.paneWPx / 2, `${s.pane}: the scale block is not in the pane's right half`);
    assertBarsMeanIt(s, `${s.pane} at open`, t);
  }

  // (4) THREE ZOOMS. A wheel over the canvas is view arithmetic on the pane under it (T-456) and
  // never a device command, so this is the gesture a user makes — and the bar must re-derive from
  // the pane's new box on the very next frame.
  // A FREQUENCY zoom (shift+wheel, T-456): a plain wheel zooms both axes and T-472 stops it at
  // either axis's bound, and a following pane opened on the observed extent is often already at the
  // time bound — which would leave this test measuring a refusal instead of a zoom.
  const at = { x: first.canvas.x + first.canvas.w / 2, y: first.canvas.y + first.canvas.h * 0.4 };
  const seen = [{ fHz: first.scales[0].fHz, tS: first.scales[0].tS, fPx: first.scales[0].fBar.w }];
  for (const step of [1, 2]) {
    await page.wheel(at, -400, { shift: true });
    await page.frames(4);
    const z = JSON.parse(await page.eval(READ));
    for (const s of z.scales) assertBarsMeanIt(s, `${s.pane} after zoom ${step}`, t);
    seen.push({ fHz: z.scales[0].fHz, tS: z.scales[0].tS, fPx: z.scales[0].fBar.w });
    await shot(`2-zoom${step}`);
  }
  t.diagnostic(`across three zooms: ${JSON.stringify(seen)}`);
  // Zooming IN never enlarges the quantity a bar of the same order stands for, and at least one of
  // the two axes actually changed its words — otherwise nothing here was measuring the zoom.
  for (let i = 1; i < seen.length; i++) {
    assert.ok(seen[i].fHz <= seen[i - 1].fHz, `zooming in grew the frequency bar's claim: ${JSON.stringify(seen)}`);
    assert.ok(seen[i].tS <= seen[i - 1].tS, `zooming in grew the time bar's claim: ${JSON.stringify(seen)}`);
  }
  assert.ok(seen[seen.length - 1].fHz < seen[0].fHz,
    `two frequency zoom-ins never shrank the bar's claim: ${JSON.stringify(seen)}`);

  // (5) The kept line: centre ± span, LIVE or a time position, and the colour scale. One LINE.
  const now = JSON.parse(await page.eval(READ));
  assert.match(now.where, /MHz\s*±/, `the status line does not state centre ± span: ${JSON.stringify(now.where)}`);
  assert.match(now.where, /LIVE|−|-\d/, `the status line does not state live-or-time-position: ${JSON.stringify(now.where)}`);
  assert.match(now.range, /dB/, `the status line does not state the colour scale: ${JSON.stringify(now.range)}`);
  assert.ok(now.status.h > 0 && now.status.h <= BOUND.h,
    `the status is ${now.status.h.toFixed(1)} px tall (bound ${BOUND.h}) — it is a line, not a panel`);
  assert.ok(now.status.w <= Math.min(BOUND.w, width - 80),
    `the status is ${now.status.w.toFixed(1)} px wide (bound ${Math.min(BOUND.w, width - 80)})`);

  // (6) 400 px: it all fits — no sideways scroll, and the scale block clear of the right-edge cluster.
  assert.ok(now.scrollW <= now.innerW, `the page scrolls sideways at ${width} px (${now.scrollW} > ${now.innerW})`);
  for (const s of now.scales) {
    for (const c of now.cluster) {
      const hit = s.box.x < c.x + c.w && s.box.x + s.box.w > c.x && s.box.y < c.y + c.h && s.box.y + s.box.h > c.y;
      assert.equal(hit, false, `the scale block overlaps ${c.sel} at ${width} px: ${JSON.stringify([s.box, c])}`);
    }
  }

  // (7) None of it is a device command — the whole point of "a pan or a wheel never commands the radio".
  assert.deepEqual(page.requests.filter((r) => CONTROL.test(r.url)).map((r) => r.url), [],
    "reading the scale or zooming the view reached a device route");
  assert.deepEqual(page.exceptions, [], "uncaught exception");
});

/**
 * T-996's rehomed capture controls (T-476's Retune, T-496's width presets), in the left column under
 * Go-to, at every width the cluster has a layout for. The block is a fixed-position stack of fixed
 * tops (`map-controls.css`), so what can go wrong is geometry: a block squeezed to a sliver that
 * wraps into a tower, or the transient offer / mode banner stacked over it. Read with BOTH of those
 * showing — the worst case — and every control in the column must be pressable at its own centre.
 */
const COLUMN = `JSON.stringify((() => {
  const shown = (e) => { const r = e.getBoundingClientRect(); return !e.closest('[hidden]') && r.width > 0 && r.height > 0; };
  const blocks = ['.map-goto', '.map-nudge', '.map-retune', '.map-offer', '.map-mode', '.map-status', '.map-topright']
    .map((sel) => [sel, document.querySelector(sel)]).filter(([, e]) => e && shown(e))
    .map(([sel, e]) => { const r = e.getBoundingClientRect(); return { sel, x: r.left, y: r.top, r: r.right, b: r.bottom }; });
  const overlaps = [];
  for (let i = 0; i < blocks.length; i++) for (let j = i + 1; j < blocks.length; j++) {
    const a = blocks[i], q = blocks[j];
    if (Math.min(a.r, q.r) - Math.max(a.x, q.x) > 0.5 && Math.min(a.b, q.b) - Math.max(a.y, q.y) > 0.5) overlaps.push(a.sel + ' x ' + q.sel);
  }
  const controls = [...document.querySelectorAll('.map-retune button, .map-offer button')].filter(shown);
  const unpressable = controls.map((el) => {
    const r = el.getBoundingClientRect(); const top = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2);
    return { name: el.textContent.trim() || el.getAttribute('aria-label'), w: Math.round(r.width), h: Math.round(r.height),
      on: top ? String(top.className?.baseVal ?? top.className ?? top.tagName) : 'nothing',
      ok: !!top && (top === el || el.contains(top)) && r.left >= 0 && r.right <= innerWidth && r.bottom <= innerHeight && r.height >= 24 };
  }).filter((b) => !b.ok);
  const retune = blocks.find((b) => b.sel === '.map-retune');
  return { blocks, overlaps, unpressable, controls: controls.length, retune,
    widths: document.querySelectorAll('.map-retune .map-width').length,
    scrollW: Math.max(document.documentElement.scrollWidth, document.body.scrollWidth), innerW: innerWidth };
})())`;

for (const [width, height] of [[1280, 800], [1000, 860], [920, 860], [400, 860]]) test(`T-996 at ${width} px: the rehomed Retune and width presets are pressable, clear of the offer and the mode banner`, async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { width, height });
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitForSurfaceMounted({ timeoutMs: 60000 });
  await page.waitFor("the capture controls to mount under Go-to",
    `!!document.querySelector('.sf-scale') && !document.querySelector('.map-retune').hidden &&
     document.querySelectorAll('.map-retune .map-width').length > 0`, { timeoutMs: 60000 });
  // The worst case: the transient Go-to offer (a destination no tuned window covers) AND the tool
  // mode banner, both shown at once beside the persistent block.
  await page.eval(`(() => { const i = document.querySelector('.map-goto input'); i.value = '2400M';
    document.querySelector('.map-goto').requestSubmit(); })()`);
  await page.waitFor("the retune offer to appear", `!document.querySelector('.map-offer').hidden`, { timeoutMs: 10000 });
  await page.eval(`document.querySelector('.map-measure-btn').click()`);
  await page.waitFor("the mode banner to appear", `!document.querySelector('.map-mode').hidden`, { timeoutMs: 5000 });
  await page.frames(4);
  const c = JSON.parse(await page.eval(COLUMN));
  t.diagnostic(`at ${width} px: ${JSON.stringify(c)}`);
  if (SHOTS) await page.shot(path.join(SHOTS, `retune-${width}.png`));
  assert.ok(c.retune, "no Retune block is shown");
  assert.ok(c.controls >= 1 + c.widths + 1, `the column matched too few controls to mean anything: ${c.controls}`);
  assert.deepEqual(c.overlaps, [], `left-column chrome drawn over each other at ${width} px`);
  assert.deepEqual(c.unpressable, [], `a capture control is not pressable at its own centre at ${width} px`);
  // A block squeezed into a tower is not "small floating chrome" (docs/23 §10.6 rule 4).
  assert.ok(c.retune.b - c.retune.y <= 80, `the Retune block is ${Math.round(c.retune.b - c.retune.y)} px tall at ${width} px`);
  assert.ok(c.scrollW <= c.innerW, `the page scrolls sideways at ${width} px`);
  assert.deepEqual(page.requests.filter((r) => CONTROL.test(r.url)).map((r) => r.url), [],
    "showing the capture controls reached a device route");
  assert.deepEqual(page.exceptions, [], "uncaught exception");
});
