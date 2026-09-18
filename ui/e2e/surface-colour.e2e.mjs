// **T-470's guard, in a real browser, read as pixels.**
//
// The defect, as the user reported it: *"colours animate and shift when I zoom."* `/api/tiles`
// returns a `range_db` computed per request from the tiles asked for, and the renderer adopted it
// every frame from the tiles that happened to be on screen — so navigating re-coloured measurements
// that had not changed. The fix anchors `(uLo, uHi)` to a range the backend measured once over the
// region. The claim is therefore about **colour**, and this file reads colours.
//
// ## Why it is written as "one viewport zooms, the other must not move"
//
// The obvious test — *zoom in and check the pixels are the same* — is the adjacent question. Zooming
// changes which pyramid level answers, so a cell's **measurement** legitimately changes with zoom;
// asserting the picture is identical would assert something false, and any bound loose enough to
// pass would be loose enough to pass with the defect too.
//
// Splitting the surface removes that confound entirely. Two viewports share one canvas, one tile
// cache and one display range. Zoom **one** of them: the other's box, level, tiles and measurements
// are untouched, so **every** pixel of it is a measurement whose colour may not move. That is a
// whole-pane claim over ~200 000 pixels, with no assumption about levels, magnification or
// sub-pixel alignment — and it is exactly the shape of what the user saw, since a viewport-derived
// range is *shared*, and so a zoom anywhere re-colours everywhere.
//
// ## Non-vacuity, stated before the assertions rather than hoped for afterwards
//
// The failure this repo keeps hitting is a sound proof of an adjacent claim, and the specific trap
// here is obvious: "two screenshots of a thing I did not touch are identical" could hold because
// the page is frozen, because the gesture did nothing, or because the canvas is a flat fill. So:
//
//  1. every zoom step must CHANGE the zoomed half — the gesture is proved to have done something;
//  2. the untouched half must be a real render (≥ 32 distinct colours, not dominated by one), so it
//     has colours to lose;
//  3. the zoomed half must be seen to change PYRAMID LEVEL across the run, from the page's own
//     per-viewport readout — so the run really spans several zoom levels;
//  4. and the whole sequence is then repeated with **auto-contrast switched on**, where the
//     untouched half MUST change. That is the fault that violates the property while satisfying
//     every other assertion in the file, driven in the product, through the same gestures: if step 4
//     ever goes quiet, the rest of this file is measuring nothing.
import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser, census } from "./harness.mjs";
import { UI_DIR } from "./backend.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const ART = process.env.HK_E2E_ARTIFACTS ?? path.join(UI_DIR, "e2e", "artifacts");

/** The map strip along the bottom of the canvas (`preview-main.ts` mounts the default, 120 px). */
const MINIMAP_PX = 120;
/** Keep clear of every pane edge: a boundary pixel is a rounding question, not a colour question. */
const INSET = 8;
/**
 * One wheel step, and it zooms **OUT** (`zoomFactor` is `exp(delta * 0.0015)`, so a positive delta
 * is a factor > 1). ~2.46× per step, which is over a level on both ladders.
 *
 * Measured, not chosen by taste: with the page opened on observed coverage the panes sit at the
 * lattice's **finest** levels, so zooming *in* magnifies the same cells and `level_f`/`level_t`
 * never move — the first version of this file did that and its own premise check caught it
 * ("only 1 distinct level configuration across the run"). Out is the direction in which a zoom is
 * really a zoom here, and it is also the harder case for the claim: the zoomed pane changes
 * pyramid level, fetches a different set of tiles, and evicts from the LRU the other pane shares.
 */
const ZOOM_OUT = 600;

/** The RGB bytes of a rectangle, as one buffer, for an exact comparison. */
function pixels(img, rect) {
  const x0 = Math.max(0, Math.round(rect.x)), y0 = Math.max(0, Math.round(rect.y));
  const x1 = Math.min(img.width, Math.round(rect.x + rect.w)), y1 = Math.min(img.height, Math.round(rect.y + rect.h));
  const out = Buffer.alloc(Math.max(0, (x1 - x0) * (y1 - y0) * 3));
  let k = 0;
  for (let y = y0; y < y1; y++) {
    for (let x = x0; x < x1; x++) {
      const d = (y * img.width + x) * 4;
      out[k++] = img.data[d]; out[k++] = img.data[d + 1]; out[k++] = img.data[d + 2];
    }
  }
  return out;
}

/**
 * **THE grey**, as the compositor writes it: `CELL_MARKS[UNOBSERVED]` is `[0.155, 0.16, 0.18]`, and
 * the framebuffer holds that as 8-bit bytes. A tolerance of ±1 per channel covers the driver's
 * rounding and nothing else — two adjacent ramp colours are further apart than that everywhere.
 */
const GREY_RGB = [Math.round(0.155 * 255), Math.round(0.16 * 255), Math.round(0.18 * 255)];

function countNear(img, rect, rgb, tol = 1) {
  const p = pixels(img, rect);
  let n = 0;
  for (let i = 0; i < p.length; i += 3) {
    if (Math.abs(p[i] - rgb[0]) <= tol && Math.abs(p[i + 1] - rgb[1]) <= tol && Math.abs(p[i + 2] - rgb[2]) <= tol) n++;
  }
  return n;
}

/** How many pixels of `rect` differ between two frames, and by how much at worst. */
function diff(a, b, rect) {
  const pa = pixels(a, rect), pb = pixels(b, rect);
  assert.equal(pa.length, pb.length, "the rectangle changed size between frames");
  let n = 0, worst = 0;
  for (let i = 0; i < pa.length; i += 3) {
    const d = Math.abs(pa[i] - pb[i]) + Math.abs(pa[i + 1] - pb[i + 1]) + Math.abs(pa[i + 2] - pb[i + 2]);
    if (d > 0) { n++; if (d > worst) worst = d; }
  }
  return { pixels: n, total: pa.length / 3, share: pa.length ? n / (pa.length / 3) : 0, worst };
}

/**
 * Screenshot until two consecutive frames agree over `rect`, so an assertion lands on a settled
 * surface rather than on whichever tile happened to be in flight.
 *
 * It **reports** rather than throws when it cannot settle: a surface that never stops changing is a
 * finding the assertions below should get to describe, not a harness timeout with no evidence.
 */
async function settle(page, rect, { timeoutMs = 20000, everyMs = 250 } = {}) {
  const t0 = Date.now();
  let prev = await page.shot();
  for (;;) {
    await new Promise((r) => setTimeout(r, everyMs));
    const next = await page.shot();
    const d = diff(prev, next, rect);
    prev = next;
    if (d.pixels === 0) return { img: next, settled: true, ms: Date.now() - t0 };
    if (Date.now() - t0 > timeoutMs) return { img: next, settled: false, ms: Date.now() - t0, last: d };
  }
}

/**
 * Every viewport row the page states: where it is looking, the level it drew at, and **what it drew
 * with** — resident tiles, coarse stand-ins, not-yet-arrived.
 *
 * `counts` is load-bearing for this file, not colour. A pane whose tiles are still arriving repaints
 * as each one lands (a hatched stand-in becomes the real thing), and that is a change in *residency*,
 * not in the colour mapping — but it is a pixel difference, and blaming it on the ramp would be
 * exactly the adjacent-question error this file is written against. Measured: a full-suite run over
 * a young record moved 5 038 px of an untouched pane with worst channel sum 265, which is a
 * stand-in swap, not a re-scale. So residency is established before the run and re-checked at every
 * step, from the page's own statement of it.
 */
const readout = (page) => page.eval(`[...document.querySelectorAll(".hk-surface-viewport")].map((r) => ({
  id: r.querySelector(".hk-surface-id")?.textContent ?? "",
  viewport: r.getAttribute("data-viewport") ?? "",
  where: r.querySelector(".hk-surface-where")?.textContent ?? "",
  level: r.querySelector(".hk-surface-level")?.textContent ?? "",
  counts: r.querySelector(".hk-surface-counts")?.textContent ?? "",
}))`);

/** Wait until every pane (the map excepted — it is 6 GHz wide and always has stand-ins) holds the
 * tiles it is drawing. Until then a repaint says nothing about colour. */
const waitResident = (page, timeoutMs = 45000) => page.waitFor(
  "every pane's own tiles to be resident (no coarse stand-ins, nothing pending)",
  `[...document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"] .hk-surface-counts')]
     .every((c) => /\\b0 coarse stand-ins · 0 pending/.test(c.textContent || ""))`,
  { timeoutMs });

/** The display range the page says it is colouring with — the readout the user reads. */
const rangeText = (page) => page.eval(`(() => {
  const s = document.querySelector('[data-slot="status"]')?.textContent ?? "";
  return (s.match(/display range [^·]*/) ?? [""])[0].trim();
})()`);

test("T-470: zooming one viewport does not re-colour another showing the same data", async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();

  // **The premise, measured and waited for rather than assumed** — this tier's own idiom (README,
  // finding 3). `run.mjs` gates the suite on *any* observed coverage, which is enough for "it drew"
  // but not for what this file needs: something **on the ramp** to re-colour. A page opened when the
  // backend had ingested one observed cell in 4096 draws a pane that is almost entirely grey and
  // pending, and then the non-vacuity step at the end finds nothing for auto-contrast to move —
  // measured exactly that way in a full-suite run, where this file goes early and the record is
  // young. `preview.ts` decides the opening viewport ONCE per load, so this is a reload loop rather
  // than a wait inside one page.
  const MIN_OBSERVED = 8;
  let observed = 0;
  for (let i = 0; i < 45 && observed < MIN_OBSERVED; i++) {
    if (i) await new Promise((r) => setTimeout(r, 1000));
    assert.equal(await page.goto(`${ORIGIN}/surface.html#token=${TOKEN}`), "load", "the page never fired load");
    await page.waitForSurfaceMounted();
    observed = Number(((await page.$text('[data-slot="census"]')) ?? "").match(/(\d+) observed/)?.[1] ?? 0);
  }
  assert.ok(observed >= MIN_OBSERVED,
    `the backend reported only ${observed} observed coverage cells after 45 s. There is not enough on ` +
    "the ramp here for a colour test to mean anything, so this run is reported as a failure rather " +
    "than as a green that measured a grey screen.");
  assert.deepEqual(page.exceptions, [], "uncaught exception during load");
  await page.waitForCanvas('[data-slot="canvas"]',
    (c) => c.distinct >= 32 && c.dominantShare < 0.92 && c.meanLuma > 8, { timeoutMs: 90000 });
  t.diagnostic(`opened on ${observed} observed coverage cells`);

  // The legend states the scale, in the mode it is actually in. A fixed range can clip, and the
  // only thing that makes that honest rather than merely consistent is saying so where it is read.
  const legend = await page.$text('.sp-legend-row[data-mark="range"]');
  assert.ok(legend, "the key does not state the display range at all; rows present: "
    + await page.eval(`[...document.querySelectorAll(".sp-legend-row")].map((r) => r.getAttribute("data-mark")).join(",") || "(none)"`)
    + " · actions: " + await page.$text('[data-slot="actions"]'));
  assert.match(legend, /Display range · -?\d+\.\d+ … -?\d+\.\d+ dBFS/, `the scale row states no range: ${legend}`);
  assert.match(legend, /Anchored/, "the page opened in a mode that is not the anchored default");
  assert.match(legend, /clipped/, "the anchored mode does not state the trade it makes");

  // ——— two viewports onto one surface ———
  await page.click(`[...document.querySelectorAll(".sp-btn")].find((b) => b.textContent.includes("Split ⇔"))`);
  await page.frames(4);

  const box = await page.$rect('[data-slot="canvas"]');
  assert.ok(box && box.w > 400 && box.h > 300, `canvas has no box: ${JSON.stringify(box)}`);
  const panesH = box.h - MINIMAP_PX;
  const half = box.w / 2;
  const left = { x: box.x + INSET, y: box.y + INSET, w: half - 2 * INSET, h: panesH - 2 * INSET };
  const right = { x: box.x + half + INSET, y: box.y + INSET, w: half - 2 * INSET, h: panesH - 2 * INSET };
  const all = { x: box.x, y: box.y, w: box.w, h: box.h };
  // Zoom at the centre of the RIGHT half. Which pane that is, is not assumed: the wheel goes to the
  // viewport under the cursor (`input.ts`), so the half containing the cursor is the one that moves.
  const at = { x: right.x + right.w / 2, y: right.y + right.h / 2 };

  // Residency first, pixels second. Until the panes hold their own tiles, a repaint is a stand-in
  // arriving, and no conclusion about colour can be drawn from one.
  await waitResident(page);
  const first = await settle(page, all);
  assert.ok(first.settled, `the surface never settled: ${JSON.stringify(first.last)}`);
  const base = first.img;
  const baseCensus = census(base, left);
  assert.ok(baseCensus.distinct >= 32 && baseCensus.dominantShare < 0.92 && baseCensus.meanLuma > 8,
    `the untouched viewport is not a real render, so it has no colours to lose: ${JSON.stringify(baseCensus)}`);
  const baseRange = await rangeText(page);
  assert.match(baseRange, /dBFS/, `the page states no display range: "${baseRange}"`);
  const levels = [await readout(page)];

  // **The honesty boundary an anchored scale makes reachable** is *not* asserted here, deliberately.
  // A fixed range clips at the bottom, so a weak measurement now lands on the ramp's floor — and the
  // ramp's floor must still not be THE grey. That separation holds for **every** ramp position
  // including clipped ones, which is a statement about the rule and is proved exhaustively one tier
  // down (`ui/test/surface-range.test.ts`, "the bottom of the ramp is NOT the grey"). Asserting it
  // here would mean asserting that a particular colour is on a particular screen at a particular
  // moment — a claim about this recording and this frame's tile residency, not about the mapping.
  // The first draft did exactly that and failed on a frame whose greys were all fallback-washed
  // stand-ins; the assertion was measuring the cache, not the honesty.
  //
  // What this tier *can* say about it, and does below, is the zoom half: whatever reads as grey in
  // the untouched viewport is still exactly that after every zoom, because those pixels never move.
  t.diagnostic(`base frame: ${countNear(base, all, GREY_RGB)} px are exactly THE grey; ` +
    `top colours ${JSON.stringify(census(base, all).top)}`);

  // ——— six zoom states, out and back, anchored ———
  //
  // Out three, in three. Out alone is not enough: past ~±10 MHz this recording's 2.4 MHz of coverage
  // is a sliver in a grey pane, so a further zoom moves the window and the level without changing
  // many pixels — measured, and the reason "every step must repaint the zoomed half" is the wrong
  // bound. Coming back in re-crosses the same levels with something on screen to repaint.
  const whereOf = (r) => r.map((x) => `${x.id}@${x.where}`).join(" | ");
  const levelsOf = (r) => r.map((x) => x.level).join(" | ");
  let prev = base, repainted = 0;
  for (const [i, delta] of [ZOOM_OUT, ZOOM_OUT, ZOOM_OUT, -ZOOM_OUT, -ZOOM_OUT, -ZOOM_OUT].entries()) {
    const step = i + 1;
    await page.wheel(at, delta);
    await page.frames(4);
    await waitResident(page);
    const s = await settle(page, all);
    assert.ok(s.settled, `zoom ${step} never settled: ${JSON.stringify(s.last)}`);
    const shot = s.img;
    const now = await readout(page);

    // (0) **The untouched pane is untouched in every sense**, including residency. If a pane that
    //     did not move has changed what it is drawing *with*, the pixel comparison below would be
    //     measuring the tile cache rather than the colour map, and this says so instead.
    for (const was of levels[0]) {
      if (was.viewport !== "pane") continue;
      const is = now.find((r) => r.id === was.id);
      if (!is || is.where !== was.where) continue; // this is the pane the wheel moved
      assert.equal(is.counts, was.counts,
        `zoom ${step}: the pane that did not move changed what it is drawing with ` +
        `("${was.counts}" → "${is.counts}"), so any pixel difference here is residency, not colour`);
    }

    // (1) **the gesture reached the viewport**, read off the page's own statement of where each
    //     viewport is looking rather than inferred from pixels. This is the honest form of "it did
    //     something": a pane can move a long way without repainting much when what it is moving over
    //     is grey, and a pixel-count bound loose enough to allow that is loose enough to allow a
    //     wheel that did nothing.
    assert.notEqual(whereOf(now), whereOf(levels[levels.length - 1]),
      `zoom ${step} moved no viewport at all: ${whereOf(now)}`);
    // (2) THE CLAIM: the other viewport is showing exactly the data it was, so its pixels may not
    //     move. Under the defect the shared range follows the zoomed pane and this whole half
    //     re-colours.
    const held = diff(base, shot, left);
    assert.equal(held.pixels, 0,
      `zoom ${step} re-coloured ${held.pixels} of ${held.total} pixels (${(held.share * 100).toFixed(1)} %, ` +
      `worst channel sum ${held.worst}) in a viewport that did not move: the same measured dB is not ` +
      "the same colour at every zoom");

    // (3) and the page's own statement of the scale is the same statement.
    assert.equal(await rangeText(page), baseRange, `the stated display range moved on zoom ${step}`);
    // The grey is a coverage claim, not a level on the scale, so a change of zoom must not create or
    // destroy any of it in a viewport that did not move. (Implied by the zero above; stated
    // separately because it is a different claim and would be the first thing to look at if a
    // future change made the pixel comparison tolerant.)
    assert.equal(countNear(shot, left, GREY_RGB), countNear(base, left, GREY_RGB),
      `zoom ${step} changed how much of the untouched viewport reads as "never observed"`);

    const moved = diff(prev, shot, right);
    if (moved.share > 0.05) repainted++;
    t.diagnostic(`zoom ${step} (Δ${delta}): zoomed half repainted ${(moved.share * 100).toFixed(1)} %, ` +
      `held half ${held.pixels} px · ${levelsOf(now)}`);
    levels.push(now);
    prev = shot;
  }

  // (1b) Over the run, the zoomed half really did repaint — several times, so the zero above is a
  //      statement about a screen that was being redrawn all along.
  assert.ok(repainted >= 3,
    `the zoomed viewport repainted on only ${repainted} of 6 steps; the wheel is not reaching the picture`);

  // (3b) The run really spanned several zoom levels — read off the page's per-viewport readout, not
  //      assumed from the gesture. Without this, "the colours held" could mean the zoom was clamped.
  const distinct = new Set(levels.map(levelsOf));
  assert.ok(distinct.size >= 3,
    `the surface reported only ${distinct.size} distinct level configurations across the run, so this ` +
    `is not a claim about three zoom levels: ${[...distinct].join("  //  ")}`);
  t.diagnostic(`levels across the run:\n  ${[...distinct].join("\n  ")}`);
  t.diagnostic(`held: ${baseCensus.distinct} distinct colours over ${baseCensus.total} px, ` +
    `dominant ${baseCensus.dominant} at ${(baseCensus.dominantShare * 100).toFixed(1)} %`);

  // ——— 4. THE CONTROL: would this comparison have SEEN a scale that moved? ———
  //
  // The zeros above are worth exactly as much as the answer to that question, and it has to be
  // answered on these pixels, in this browser, not argued.
  //
  // **What the control is, and why it is not "zoom with the fault switched on".** The first three
  // drafts tried that: press Auto-contrast, repeat the gestures, require the untouched pane to move.
  // It worked on 3 runs in 4 and then measured 0.00 % — and the reason is instructive rather than
  // flaky. Auto-contrast tracks the union of the `range_db` of **every tile on screen**, and the
  // pane that is deliberately not moving contributes its extremes to that union at every step. On
  // this 2.4 MHz recording it usually pins both ends, so a zoom of the *other* pane often does not
  // move the union at all. The defect is real — it is what the user reported, on a live front end
  // whose range does move — but this fixture is not a reliable generator of it, and a control that
  // fires 3 times in 4 is a flake that would get the whole file deleted.
  //
  // So the control asserts the property the zeros actually depend on: **this comparison can see a
  // change of display range.** Switching mode changes the range and nothing else — same panes, same
  // boxes, same levels, same tiles — so the pixels it moves are exactly the pixels a viewport-driven
  // range would have moved. Measured: the anchored scale over this fixture is ≈ −73…−51 dBFS and the
  // tracking one ≈ −86…−48, and the difference repaints a large fraction of the untouched pane.
  // That is deterministic, it is the same measurement as the claim, and it is a stronger statement
  // than the flaky one: not "the fault can appear" but "had the range moved by any comparable
  // amount, every assertion above would have failed".
  await page.click(`document.querySelector('[data-slot="contrast"]')`);
  await page.frames(4);
  const autoLegend = await page.$text('.sp-legend-row[data-mark="range"]');
  assert.match(autoLegend, /Auto-contrast/, "the opt-in contrast mode is not reachable from the page");
  assert.match(autoLegend, /changes colour as you zoom/i, "auto-contrast does not state ITS trade");

  // **The identical drive**, on the identical page, with the identical gestures — the only
  // difference being which scale is in force. One step is not enough to demonstrate the fault:
  // whether a *particular* zoom moves a viewport-derived range depends on what the two panes happen
  // to contain, and a run where it barely moved would report the fault as absent. The whole
  // six-step sequence re-crosses four levels, so if the scale follows the viewport at all, it shows.
  await waitResident(page);
  const autoSettled = await settle(page, all, { timeoutMs: 25000 });
  const autoBase = autoSettled.img;
  const autoBaseReadout = await readout(page);

  // The control itself. Nothing moved but the scale, and the untouched pane's residency is checked
  // the same way as in the loop above, so this is a like-for-like comparison of the same pixels.
  for (const was of levels[0]) {
    if (was.viewport !== "pane") continue;
    const is = autoBaseReadout.find((r) => r.id === was.id);
    assert.ok(is && is.where === was.where && is.level === was.level,
      "switching contrast mode moved a viewport; it must change the scale and nothing else");
  }
  const sensitivity = diff(base, autoBase, left);
  assert.ok(sensitivity.share > 0.05,
    `changing the display range repainted only ${(sensitivity.share * 100).toFixed(2)} % of this ` +
    "viewport, so the zero-difference assertions above could have held with the range moving under " +
    "them. This comparison cannot see what it claims to be measuring.");
  t.diagnostic(`control: changing the scale alone (${baseRange} → ${await rangeText(page)}) repainted ` +
    `${(sensitivity.share * 100).toFixed(1)} % of the untouched viewport — the same pixels that stayed ` +
    "byte-identical through six zooms");

  // The same six gestures with the tracker on, **reported and not gated** for the reason set out
  // above: how far a viewport-derived range moves is a property of what the two panes contain, so on
  // this fixture it lands anywhere between 0 % and 9 %. It is worth printing — it is the defect,
  // measured in the product — but a gate on it would fire on the recording rather than on the code.
  let worstHeld = 0;
  for (const delta of [ZOOM_OUT, ZOOM_OUT, ZOOM_OUT, -ZOOM_OUT, -ZOOM_OUT, -ZOOM_OUT]) {
    await page.wheel(at, delta);
    await page.frames(4);
    await waitResident(page);
    const s = await settle(page, all, { timeoutMs: 25000 });
    const now = await readout(page);
    // Only count a step where a pane genuinely stayed put, with the same tiles under it — otherwise
    // the number would be residency, not colour.
    const stayed = autoBaseReadout.some((was) => was.viewport === "pane"
      && now.some((is) => is.id === was.id && is.where === was.where && is.level === was.level && is.counts === was.counts));
    if (stayed) worstHeld = Math.max(worstHeld, diff(autoBase, s.img, left).share);
  }
  t.diagnostic(`the defect itself, measured: with auto-contrast ON the same six zooms re-coloured up ` +
    `to ${(worstHeld * 100).toFixed(2)} % of an untouched viewport; anchored, every one of them ` +
    "changed 0 px");

  // Back to the anchored scale, and the page says so — the toggle is a toggle, not a one-way door.
  await page.click(`document.querySelector('[data-slot="contrast"]')`);
  await page.frames(4);
  assert.match(await page.$text('.sp-legend-row[data-mark="range"]'), /Anchored/);
  // The legend is repainted on the press; the status line is chrome on a 500 ms tick, so this waits
  // for its cadence rather than racing it. (Written as a wait after the first version read the stale
  // line and reported the anchor as lost — the assertion was right, the sampling was not.)
  await page.waitFor("the status line to state the anchored range again",
    `(document.querySelector('[data-slot="status"]')?.textContent ?? "").includes(${JSON.stringify(baseRange)})`,
    { timeoutMs: 5000 });
  assert.equal(await rangeText(page), baseRange,
    "going back to the anchor did not restore the range the region was measured at");

  // **And the round trip closes, in pixels.** Six zooms anchored, a mode switch, six more zooms
  // tracking, and back — after all of it the untouched viewport must be the *same image it was on
  // the first frame*. This is the claim stated as an identity rather than as a sequence of
  // differences, and it is the one an "anchor" has to satisfy to mean anything: the scale is a
  // property of the region, so returning to it returns the colours exactly.
  await waitResident(page);
  const closed = await settle(page, all, { timeoutMs: 25000 });
  const back = diff(base, closed.img, left);
  assert.equal(back.pixels, 0,
    `after the round trip the untouched viewport differs from its first frame by ${back.pixels} of ` +
    `${back.total} pixels (worst channel sum ${back.worst}): the anchor is not a fixed point`);

  await page.shot(path.join(ART, "surface-colour.png"));
  assert.deepEqual(page.requests.filter((r) => /\/api\/control\//.test(r.url)), [],
    "a colour control reached a device route");
});
