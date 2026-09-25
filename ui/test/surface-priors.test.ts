// T-812 (MAP-12): the band-plan priors layer (docs/24 §3f/§7) — suggestions, never truth.
//
// The claims, each against the degenerate implementation that would otherwise pass:
//  1. **The request the client builds is the pane's own window** — `GET /api/priors` with all four
//     of `f_lo`, `f_hi`, `t0`, `t1` from the pane box (a client asking for the wrong thing is the
//     defect no server contract test catches, CLAUDE.md), quantized so a sub-pixel pan re-uses it.
//  2. **The served ranking and wording are kept as served** — no re-ranking, no client-side reason.
//  3. **A prior is a dashed stroke, never a wash or a detection's outline**: every quad is
//     `prior-band`, thin in one dimension, and an allocation edge outside the pane draws nothing.
//     Strokes are laid out through the pane's box, so a pan moves them with the rows.
//  4. **Every label says "prior"**, and the off-raster flag is the backend's, "flagged, not snapped".
//  5. **The layer is off by default and additive**: with it off nothing is drawn; turning it on adds
//     prior quads without removing a single detection quad.
//  6. **Thin client / no device route**: the layer module fetches nothing, names no route but its own
//     reference-data GET, and toggling it reaches no route.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  MAX_PRIOR_LABELS, PRIOR_INK, parsePriors, priorLabels, priorQuads, priorsPath, type Prior,
} from "../src/surface/priors";
import { composeOverlays, defaultPaneLayers, isLayerVisible, withLayer, type OverlayLayerFn } from "../src/surface/layers";
import { quadSizePx, type OverlayQuad } from "../src/surface/minimap";
import type { Box } from "../src/surface/lattice";
import type { PaneRect, PaneView } from "../src/surface/surface";
import { toClip } from "../src/surface/surface";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";
import { paneLayersOf, setPaneLayer } from "../src/app/map/layers-slice";

const S = 1e9;
const box: Box = { f0Hz: 86e6, f1Hz: 110e6, t0Ns: 1_790_000_000 * S, t1Ns: 1_790_000_600 * S };
const rect: PaneRect = { x: 0, y: 0, w: 1200, h: 600 };

/** A served `/api/priors` body, shaped exactly as `docs/api.md` documents it. */
const served = {
  kind: "suggestion",
  statement: "Band-plan priors are suggestions, never truth: …",
  truncated: false,
  priors: [
    {
      rank: 1, id: "fm-broadcast", f_lo_hz: 88e6, f_hi_hz: 108e6, service: "broadcasting",
      allocation: "non-federal", source: "us-47cfr2106-compact:fm-broadcast", support: "cited",
      off_raster_hz: 150e3, unverified: false,
      reason: "fm-broadcast is allocated primary to broadcasting … 1 emission sits off the 200.0 kHz raster: flagged, not snapped. A suggestion, never truth.",
    },
    {
      rank: 2, id: "aviation-vor-ils", f_lo_hz: 108e6, f_hi_hz: 117.975e6, service: "aeronautical-radionavigation",
      allocation: "shared", source: "us-47cfr2106-compact:aviation-vor-ils", support: "context",
      off_raster_hz: null, unverified: false,
      reason: "aviation-vor-ils is allocated … No measured emission in this window lies in it: context only. A suggestion, never truth.",
    },
    { rank: 3, id: "broken", f_lo_hz: "x", f_hi_hz: 1 },
  ],
};

const rows = (): readonly Prior[] => parsePriors("/api/priors?x", served)!.rows;

test("MAP-12: the client asks for the pane's own window — all four bounds, quantized", () => {
  const p = priorsPath(box)!;
  const u = new URL(p, "http://h");
  assert.equal(u.pathname, "/api/priors");
  const q = Object.fromEntries(u.searchParams);
  assert.deepEqual(Object.keys(q).sort(), ["f_hi", "f_lo", "t0", "t1"]);
  const [fLo, fHi, t0, t1] = [q.f_lo, q.f_hi, q.t0, q.t1].map(Number);
  assert.ok(fLo <= box.f0Hz && fHi >= box.f1Hz, `the request covers the pane in frequency: ${p}`);
  assert.ok(fHi - fLo < (box.f1Hz - box.f0Hz) * 1.05, `and not much more: ${p}`);
  assert.ok(t0 <= box.t0Ns / S && t1 >= box.t1Ns / S && t0 > 1.7e9, `absolute capture seconds, covering the pane: ${p}`);
  // A sub-pixel pan asks the same question (no refetch per frame); a real pan asks a new one.
  const nudged = { ...box, f0Hz: box.f0Hz + 1e3, f1Hz: box.f1Hz + 1e3 };
  assert.equal(priorsPath(nudged), p);
  const panned = { ...box, f0Hz: box.f0Hz + 5e6, f1Hz: box.f1Hz + 5e6 };
  assert.notEqual(priorsPath(panned), p);
  assert.equal(priorsPath({ ...box, f1Hz: box.f0Hz }), null, "a degenerate box sends nothing");
  assert.equal(priorsPath({ ...box, t1Ns: box.t0Ns }), null);
  assert.ok(Number(new URL(priorsPath({ ...box, f0Hz: -1e6 })!, "http://h").searchParams.get("f_lo")) >= 0);
});

test("MAP-12: the served ranking and reasons are kept as served; malformed rows are dropped", () => {
  const a = parsePriors("/api/priors?x", served)!;
  assert.deepEqual(a.rows.map((r) => r.id), ["fm-broadcast", "aviation-vor-ils"]);
  assert.equal(a.rows[0].reason, served.priors[0].reason);
  assert.equal(a.rows[0].offRasterHz, 150e3);
  assert.equal(a.rows[1].offRasterHz, null);
  assert.equal(a.statement, served.statement);
  assert.equal(parsePriors("p", { error: "no" }), null);
  assert.equal(parsePriors("p", null), null);
});

test("MAP-12: priors are dashed strokes through the pane's box — never a wash, never outside the pane", () => {
  const qs = priorQuads(rows(), box, rect);
  assert.ok(qs.length > 10, "dashed: many short strokes");
  for (const q of qs) {
    assert.equal(q.kind, "prior-band");
    assert.ok(q.id.startsWith("prior:"));
    const { wPx, hPx } = quadSizePx(q, rect);
    assert.ok(Math.min(wPx, hPx) <= 3.01, `a stroke, not a wash: ${wPx}×${hPx}`);
    assert.ok(q.clip[0] >= -1 && q.clip[2] <= 1 && q.clip[1] >= -1 && q.clip[3] <= 1, "inside the pane");
  }
  // Each edge sits where the data pass puts that frequency (toClip), so it moves with the rows.
  const xOf = (f: number) => toClip({ f0Hz: f, f1Hz: f, t0Ns: box.t0Ns, t1Ns: box.t1Ns }, box)[0];
  const tall = qs.filter((q) => quadSizePx(q, rect).hPx > quadSizePx(q, rect).wPx);
  for (const f of [88e6, 108e6]) {
    assert.ok(tall.some((q) => Math.abs((q.clip[0] + q.clip[2]) / 2 - xOf(f)) < 0.01), `an edge at ${f}`);
  }
  // 117.975 MHz is past the pane's right edge: nothing is drawn for it, not a stroke on the border.
  assert.ok(!tall.some((q) => q.clip[2] > 0.999), "an out-of-pane edge was pinned to the right border");
  // Pan so 88 MHz leaves the pane: its edge draws nothing rather than pinning to the border.
  const right: Box = { ...box, f0Hz: 90e6, f1Hz: 114e6 };
  const edges = priorQuads(rows(), right, rect).filter((q) => quadSizePx(q, rect).hPx > quadSizePx(q, rect).wPx);
  const xR = (f: number) => toClip({ f0Hz: f, f1Hz: f, t0Ns: right.t0Ns, t1Ns: right.t1Ns }, right)[0];
  assert.ok(!edges.some((q) => Math.abs(q.clip[0] - -1) < 0.002), "no edge pinned to the left border");
  assert.ok(edges.some((q) => Math.abs((q.clip[0] + q.clip[2]) / 2 - xR(108e6)) < 0.01), "108 MHz moved with the pan");
  assert.deepEqual(priorQuads(rows(), { ...box, f0Hz: 2e9, f1Hz: 2.1e9 }, rect), [], "no allocation in view, nothing drawn");
  // Never a detection's solid colour at full strength.
  assert.ok(PRIOR_INK[3] < 0.9);
});

test("MAP-12: every label says 'prior'; the off-raster flag is the backend's, flagged not snapped", () => {
  const ls = priorLabels("p1", rows(), box, rect, 600, 1);
  assert.equal(ls.length, 2);
  for (const l of ls) {
    assert.match(l.text, /^prior #\d+ · /);
    assert.ok(l.reason.includes("never truth"));
  }
  assert.equal(ls[0].text, "prior #1 · fm-broadcast");
  assert.ok(ls[0].offRaster && /150\.0 kHz off raster — flagged, not snapped/.test(ls[0].sub), ls[0].sub);
  assert.ok(!ls[1].offRaster && /context only/.test(ls[1].sub), ls[1].sub);
  // A band too narrow on screen is stroked but not labelled.
  const narrow: Prior = { ...rows()[1], id: "sliver", fLoHz: 100e6, fHiHz: 100.01e6, rank: 3 };
  assert.equal(priorLabels("p1", [narrow], box, rect, 600, 1).length, 0);
  const many = Array.from({ length: 12 }, (_, i) => ({ ...rows()[0], rank: i + 1, id: `a${i}` }));
  assert.equal(priorLabels("p1", many, box, rect, 600, 1).length, MAX_PRIOR_LABELS);
  // Labels sit over the band's visible centre.
  const x = ls[0].x;
  assert.ok(Math.abs(x - ((98e6 - box.f0Hz) / (box.f1Hz - box.f0Hz)) * rect.w) < 1, `centred: ${x}`);
});

test("MAP-12: the layer is off by default and additive — it never removes a detection", () => {
  const pane = { id: "p1", box, rect } as unknown as PaneView;
  const det: OverlayQuad = { clip: [-0.1, -0.1, 0.1, 0.1], rgba: [1, 1, 1, 1], kind: "signal-box", id: "e1" };
  const fns: Record<string, OverlayLayerFn> = {
    detections: () => [det],
    priors: (p) => priorQuads(rows(), p.box, p.rect),
  };
  const off = defaultPaneLayers("p1");
  assert.equal(isLayerVisible(off, "priors"), false, "priors default off");
  assert.deepEqual(composeOverlays(off, fns, pane, 0), [det]);
  const on = composeOverlays(withLayer(off, "priors", true), fns, pane, 0);
  assert.ok(on.includes(det), "turning priors on removed a detection");
  assert.ok(on.some((q) => q.kind === "prior-band"));
  // z 60, on top: the priors come after the detection in the one list.
  assert.ok(on.indexOf(det) < on.findIndex((q) => q.kind === "prior-band"));
});

test("MAP-12: thin client — the layer module fetches nothing, and toggling it reaches no route", () => {
  const src = readFileSync("src/surface/priors.ts", "utf8").replace(/\/\/.*$/gm, "");
  for (const banned of ["fetch(", "client.", "DeviceAction", "/api/control", "/api/device", "retune"]) {
    assert.ok(!src.includes(banned), `priors.ts reaches for ${banned}`);
  }
  assert.deepEqual([...new Set([...src.matchAll(/\/api\/[a-z/]+/g)].map((m) => m[0]))], ["/api/priors"], "priors.ts names a route but its own");
  // Toggling the layer is store state only.
  const g = globalThis as { fetch?: unknown };
  const real = g.fetch;
  const fetched: unknown[] = [];
  g.fetch = (...a: unknown[]) => { fetched.push(a); return Promise.reject(new Error("toggle reached the network")); };
  try {
    const store = createStore(initialState());
    const tpl = defaultPaneLayers("t");
    store.set(setPaneLayer("p1", "priors", true, tpl));
    assert.equal(isLayerVisible(paneLayersOf(store.get(), "p1", tpl), "priors"), true);
    store.set(setPaneLayer("p1", "priors", false, tpl));
  } finally { if (real) g.fetch = real; else delete g.fetch; }
  assert.deepEqual(fetched, []);
  // The host fetches priors on the poll (never in the frame), through the one client, GET only.
  const host = readFileSync("src/app/centre/surface.ts", "utf8");
  assert.match(host, /startPoll\(async \(\) => \{ mirror\(\); refreshPriors\(\); refreshDensity\(\); \}, 1000\)/);
  assert.match(host, /client\.get<unknown>\(path\)/);
  assert.ok(!/client\.(post|put|delete)[^;]*priors/i.test(host), "priors must only ever be read");
});
