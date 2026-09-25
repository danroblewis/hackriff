// T-825 (MAP-25): **the request the client BUILDS** — docs/23 §11 rule 3, made enforceable.
//
// The gap this closes is T-367's: `crates/hk-cli/tests/api_contract.rs` proves the *server* serves a
// route correctly, and every suite stayed green while the client asked `GET /api/timeline` with no
// band and drew an empty canvas. No contract test can see that, because the wrong question is a
// well-formed request. So this file asserts the questions, not the answers:
//
//  1. Each map-UI panel's own request builder is called, and every path it produces is checked
//     against `docs/api.md`'s required parameters (a windowed read carries all four window
//     parameters, ordered and finite) — the T-367 shape is a red proof below.
//  2. Every path built is matched against the **routes docs/23 §11 declares**. A client that starts
//     asking for a route the design does not name is red here, in `ui/test`, not on staging.
//  3. The §11 table is covered *both ways*: a declared route is either driven here, or named with
//     the test that does assert its request, or pinned as "no client builds it yet" — and that pin
//     is itself checked against `ui/src`, so the day a client starts building it this file goes red
//     until someone asserts its shape.
//
// Nothing here renders: the builders are pure or take a recording client, so the file needs no DOM
// and states only what leaves the browser.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import type { AppContext } from "../src/app/context";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";
import { drawerRequests, observationsRequest, type DrawerScope } from "../src/app/chrome/explore-drawer";
import { annotationsPath, boxRequest, commitAnnotation } from "../src/app/explore/annotate";
import { commitMeasurement, type MeasureView } from "../src/app/explore/measure";
import { collectionPath, exportPath, loadResearch, rowPath } from "../src/app/map/research";
import { fetchSignatureMatch } from "../src/app/explore/signature";
import { eventsQuery, trackQuery, type Region } from "../src/app/history/catalogue";
import { deleteEntry, inventoryQuery, promoteEntry } from "../src/inventory";
import { timelineRequest } from "../src/navigators";
import { coverageUrl } from "../src/surface/bootstrap";
import { densityUrl } from "../src/surface/density";
import { priorsPath } from "../src/surface/priors";
import { tileUrl, type Box, type TileAddr } from "../src/surface/lattice";

const DOC = "../docs/23-map-ui-philosophy.md";
const S_TO_NS = 1_000_000_000;

// ---------------------------------------------------------------------------
// docs/23 §11: the declared routes
// ---------------------------------------------------------------------------

/** The `Reads`/`Writes` routes of the §11 table, as segment patterns (`{id}` matches any segment). */
function declaredRoutes(md: string): string[][] {
  const from = md.indexOf("## 11. Panel -> state -> route");
  assert.ok(from >= 0, "docs/23 §11 not found — this guard reads the table, so a rename must be seen");
  const sec = md.slice(from, md.indexOf("## Sources", from));
  const rows = sec.split("\n").filter((l) => l.startsWith("|"));
  assert.ok(rows.length >= 1, "the §11 table has rows");
  const out = new Map<string, string[]>();
  for (const line of rows) {
    for (const m of line.matchAll(/(\/(?:api|ws)\/[A-Za-z0-9_/{}.-]*)/g)) {
      const pat = m[1].replace(/[.,`]+$/, "").replace(/\/$/, "");
      const segs = pat.split("/").slice(1).map((s) => (s.startsWith("{") ? "*" : s));
      out.set(segs.join("/"), segs);
    }
  }
  return [...out.values()];
}

const patternKey = (segs: readonly string[]) => `/${segs.join("/")}`;

/** The declared pattern `path` asks for, or null — segment-wise, so `{id}` is a wildcard. */
function matchDeclared(path: string, declared: readonly string[][]): string | null {
  const segs = path.split("?")[0].split("/").slice(1);
  for (const d of declared) {
    if (d.length !== segs.length) continue;
    if (d.every((s, i) => s === "*" || s === segs[i])) return patternKey(d);
  }
  return null;
}

// ---------------------------------------------------------------------------
// docs/api.md: what each request must carry
// ---------------------------------------------------------------------------

const WINDOW4 = ["f_lo", "f_hi", "t0", "t1"];
/** Required query parameters per route, from docs/api.md's own "required" wording. */
const REQUIRED: Readonly<Record<string, readonly string[]>> = {
  "/api/events": WINDOW4,
  "/api/coverage": [...WINDOW4, "cells"],
  "/api/annotations": [...WINDOW4, "limit"],
  "/api/priors": WINDOW4,
  "/api/observations": [...WINDOW4, "limit"],
  "/api/analysis/strongest": ["f_lo", "f_hi", "window_s"],
  "/api/scheduler": WINDOW4,
  "/api/inventory": ["state", ...WINDOW4],
  "/api/inventory/*/presence": ["t0", "t1"],
  "/api/timeline": ["f_lo", "f_hi", "columns", "rows"], // the T-367 bug: a bandless timeline request
  "/api/tiles": ["level_f", "level_t", "f_index", "t_index"],
  "/api/tiles/events": ["level_f", "level_t", "f_index", "t_index"],
  "/api/signatures/match": ["emitter"],
};

interface Built { method: "GET" | "POST" | "PUT" | "DELETE"; path: string; body?: unknown }

/** Asserts one built request: a declared route, sane values, and every required parameter present. */
function requireShape(b: Built, declared: readonly string[][]): void {
  const where = `${b.method} ${b.path}`;
  const pattern = matchDeclared(b.path, declared);
  assert.ok(pattern !== null, `${where}: the client builds a route docs/23 §11 does not declare`);
  for (const bad of ["undefined", "NaN", "[object", "null", "%7B"]) {
    assert.ok(!b.path.includes(bad), `${where}: "${bad}" reached the wire`);
  }
  const q = new URLSearchParams(b.path.split("?")[1] ?? "");
  for (const [k, v] of q) {
    assert.notEqual(v, "", `${where}: empty ${k}`);
    if (/^-?\d/.test(v)) assert.ok(Number.isFinite(Number(v.split(",")[0])), `${where}: ${k}=${v}`);
  }
  const ord = (lo: string, hi: string) => {
    if (q.has(lo) && q.has(hi)) assert.ok(Number(q.get(lo)) < Number(q.get(hi)), `${where}: ${lo} >= ${hi}`);
  };
  ord("f_lo", "f_hi");
  ord("t0", "t1");
  if (b.method !== "GET") return; // a write's shape is its body; docs/api.md names no query on one
  for (const need of REQUIRED[pattern!] ?? []) {
    assert.ok(q.has(need), `${where}: missing the required ${need} (docs/api.md ${pattern})`);
  }
}

// ---------------------------------------------------------------------------
// The drivers: each calls the client's OWN builder and reports what it built
// ---------------------------------------------------------------------------

const T0 = 1_700_000_000, T1 = T0 + 1800;
const BAND = { loHz: 88e6, hiHz: 108e6 };
const REGION: Region = { fLoHz: BAND.loHz, fHiHz: BAND.hiHz, t0: T0, t1: T1 };
const BOX: Box = { f0Hz: BAND.loHz, f1Hz: BAND.hiHz, t0Ns: T0 * S_TO_NS, t1Ns: T1 * S_TO_NS };
const ADDR: TileAddr = { device: "any", scheme: "view", levelF: 3, levelT: 2, fIndex: 11, tIndex: 7, cells: 256 };
const SCOPE: DrawerScope = { loHz: BAND.loHz, hiHz: BAND.hiHz, t0: T0, t1: T1, region: null };
const VIEW: MeasureView = { center_hz: 98e6, span_hz: 20e6, t_capture: [T0, T1], tier: "live-iq" };
const DRAG = { f0Hz: 97e6, f1Hz: 99e6, t0Ns: T0 * S_TO_NS, t1Ns: (T0 + 2) * S_TO_NS };
const ID = "d3b07384-d9a0-4c9b-8f4a-000000000001";

/** A client that answers every call with a body wide enough for the loaders, and records the call. */
function recorder() {
  const calls: Built[] = [];
  const body = () => ({
    collections: [], markers: [], annotations: [], measurements: [], views: [], next_cursor: null,
    emitter: ID, match: null, history: [], id: ID, kind: "delta_f",
  });
  const rec = (method: Built["method"]) => async (path: string, b?: unknown) => {
    calls.push({ method, path, body: b });
    return body();
  };
  const client = { get: rec("GET"), post: rec("POST"), put: rec("PUT"), del: rec("DELETE") };
  const ctx = { client, store: createStore(initialState()) } as unknown as AppContext;
  return { calls, ctx, client: client as unknown as Parameters<typeof promoteEntry>[0] };
}

const DRIVERS: readonly { panel: string; run: () => Promise<Built[]> }[] = [
  {
    panel: "left inventory column (candidate/confirmed lists, promote, delete)",
    run: async () => {
      const { calls, client } = recorder();
      const f = { fLoHz: BAND.loHz, fHiHz: BAND.hiHz, t0: T0, t1: T1, relations: "all" as const };
      await promoteEntry(client, ID, async () => {});
      await deleteEntry(client, ID, async () => {});
      return [
        { method: "GET", path: inventoryQuery("candidate", f) },
        { method: "GET", path: inventoryQuery("confirmed", f) },
        ...calls,
      ];
    },
  },
  {
    panel: "bottom sheet — Explore tab (drawer + past surveys)",
    run: async () => {
      const r = drawerRequests(SCOPE);
      return [r.events, r.strongest, r.scheduler, r.coverage, observationsRequest(T0, T1, 200, 0)]
        .map((path) => ({ method: "GET" as const, path }));
    },
  },
  {
    panel: "bottom sheet — Selected tab (presence, events, signature match)",
    run: async () => {
      const { calls, client } = recorder();
      await fetchSignatureMatch(client as unknown as Parameters<typeof fetchSignatureMatch>[0], ID);
      return [
        { method: "GET", path: trackQuery(ID, REGION) },
        { method: "GET", path: eventsQuery(REGION, "confirmed") },
        ...calls,
      ];
    },
  },
  {
    panel: "canvas layers — tiles, density, coverage fog, timeline, priors",
    run: async () => {
      const priors = priorsPath(BOX);
      assert.ok(priors !== null, "a real box must produce a priors request");
      return [tileUrl(ADDR), densityUrl(ADDR), coverageUrl(BOX, 64, 32), timelineRequest(BAND, 256, 128), priors]
        .map((path) => ({ method: "GET" as const, path }));
    },
  },
  {
    panel: "annotations (authoring + the windowed read)",
    run: async () => {
      const { calls, ctx } = recorder();
      const req = boxRequest(DRAG, "a note", VIEW);
      assert.ok(req !== null, "a real drag must produce an annotation body");
      await commitAnnotation(ctx, req);
      return [{ method: "GET", path: annotationsPath({ f0Hz: BAND.loHz, f1Hz: BAND.hiHz, t0S: T0, t1S: T1 }) }, ...calls];
    },
  },
  {
    panel: "measurements (cursors only — the server computes the value)",
    run: async () => {
      const { calls, ctx } = recorder();
      await commitMeasurement(ctx, DRAG, VIEW, () => "Δf");
      assert.ok(calls.length > 0, "a real drag must post a measurement");
      for (const c of calls) {
        const b = c.body as Record<string, unknown>;
        assert.ok(!("value" in b) && !("unit" in b), "a measurement's value is never the client's");
        assert.ok(Array.isArray(b.cursors) && (b.cursors as unknown[]).length === 2, "two cursors");
      }
      return calls;
    },
  },
  {
    panel: "Research slide-in (collections, markers, annotations, export)",
    run: async () => {
      const { calls, ctx } = recorder();
      await loadResearch((p) => ctx.client.get(p));
      return [
        ...calls,
        { method: "PUT", path: collectionPath(ID) },
        { method: "DELETE", path: collectionPath(ID) },
        { method: "PUT", path: rowPath({ kind: "marker", id: ID }) },
        { method: "DELETE", path: rowPath({ kind: "annotation", id: ID }) },
        { method: "GET", path: exportPath(null) },
        { method: "GET", path: exportPath(ID) },
      ];
    },
  },
];

/** Declared routes asserted in another suite, with the file that asserts the request it builds. */
const ASSERTED_ELSEWHERE: Readonly<Record<string, string>> = {
  "/api/navigation": "test/app-centre.test.ts",
  "/api/control/center": "test/app-centre.test.ts",
  "/api/analyze": "test/app-analyze-panel.test.ts",
  "/api/outputs/record/start": "test/outputs.test.ts",
  "/api/streams": "test/app-inspector.test.ts",
  "/api/recipes/match": "test/app-decode.test.ts",
  "/api/inventory/*": "test/inventory.test.ts",
  "/ws/open/listen": "test/app-shell.test.ts",
  "/ws/presence": "test/presence.test.ts",
};

/** Declared routes no client builds yet, with why. Checked against `ui/src`: the day one is built,
 * this file is red until its request shape is asserted above. */
const NO_CLIENT_YET: Readonly<Record<string, string>> = {
  "/api/views": "T-819 serves the store; the Research slide-in has no Views tab yet (MAP-19 FE)",
  "/api/views/*": "as /api/views: no Views tab builds a write yet (MAP-19 FE)",
  "/api/measurements/*": "MAP-22 draws and posts measurements; editing/deleting one is not built yet",
  "/api/collections/*/markers": "the client posts markers through the authoring gesture, not this list route",
  "/api/history": "served, but no client reads it since T-445 retired the spectrum-grid pane",
  "/api/inventory/*/classification": "served inside the /api/inventory row the sheet already reads",
};

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

const declared = declaredRoutes(readFileSync(DOC, "utf8"));
assert.ok(declared.length > 20, "the real §11 table declares every panel's routes — a thin parse is no guard");

test("T-825: every request a map-UI panel builds is a declared route, shaped as docs/api.md requires", async () => {
  for (const d of DRIVERS) {
    const built = await d.run();
    assert.ok(built.length > 0, `${d.panel}: no request built — the driver proves nothing`);
    for (const b of built) {
      try {
        requireShape(b, declared);
      } catch (e) {
        assert.fail(`${d.panel}: ${(e as Error).message}`);
      }
    }
  }
});

test("T-825: docs/23 §11 is covered both ways — every declared route is driven, named or pinned", async () => {
  const hit = new Set<string>();
  for (const d of DRIVERS) for (const b of await d.run()) hit.add(matchDeclared(b.path, declared)!);
  const gaps: string[] = [];
  for (const d of declared) {
    const key = patternKey(d);
    if (hit.has(key) || key in ASSERTED_ELSEWHERE || key in NO_CLIENT_YET) continue;
    gaps.push(key);
  }
  assert.deepEqual(gaps, [], "docs/23 §11 rule 3: a declared route with no request-shape assertion");
  // The other direction: a stale entry is a lie about coverage.
  for (const [key, file] of Object.entries(ASSERTED_ELSEWHERE)) {
    assert.ok(declared.some((d) => patternKey(d) === key), `${key}: named here but no longer in §11`);
    const src = readFileSync(file, "utf8");
    assert.ok(src.includes(key.replace("/*", "/")), `${file} no longer asserts a ${key} request`);
  }
  for (const key of Object.keys(NO_CLIENT_YET)) {
    assert.ok(declared.some((d) => patternKey(d) === key), `${key}: pinned but no longer in §11`);
  }
});

/** Every `/api|/ws` literal a module builds, with `${…}` segments as wildcards. */
function routeLiteralsIn(src: string): string[] {
  const code = src.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
  const out = new Set<string>();
  for (const m of code.matchAll(/(\/(?:api|ws)\/[A-Za-z0-9_/.-]*(?:\$\{[^}]*\}[A-Za-z0-9_/.-]*)*)/g)) {
    // A `${…}` standing for a whole segment is a wildcard; one glued to the end of a segment is a
    // query or a suffix the route does not care about, and is dropped.
    const pat = m[1].replace(/\$\{[^}]*\}/g, "*").replace(/[?`'"].*$/, "").replace(/([^/])\*+$/, "$1").replace(/\/$/, "");
    if (pat.split("/").length > 2) out.add(pat);
  }
  return [...out];
}

const MAP_MODULES = [...readdirSync("src/app/map"), ...readdirSync("src/app/chrome")]
  .filter((f) => f.endsWith(".ts"))
  .map((f) => (readdirSync("src/app/map").includes(f) ? `src/app/map/${f}` : `src/app/chrome/${f}`));

test("T-825: no map-UI panel module builds a route docs/23 §11 does not declare", () => {
  assert.ok(MAP_MODULES.length >= 5, "the module list resolved");
  for (const file of MAP_MODULES) {
    for (const lit of routeLiteralsIn(readFileSync(file, "utf8"))) {
      assert.ok(matchDeclared(lit.replace(/\*/g, "x"), declared) !== null,
        `${file} builds ${lit}, which docs/23 §11 does not declare — add the row or stop asking`);
    }
  }
});

test("T-825: a route pinned as 'no client yet' is built by NO module under ui/src", () => {
  const all: string[] = [];
  const walk = (dir: string) => {
    for (const e of readdirSync(dir, { withFileTypes: true })) {
      if (e.isDirectory()) walk(`${dir}/${e.name}`);
      else if (e.name.endsWith(".ts")) all.push(`${dir}/${e.name}`);
    }
  };
  walk("src");
  const built = new Set(all.flatMap((f) => routeLiteralsIn(readFileSync(f, "utf8"))));
  for (const [key, why] of Object.entries(NO_CLIENT_YET)) {
    const asLiteral = key.replace(/\*/g, "x");
    for (const lit of built) {
      assert.notEqual(matchDeclared(lit.replace(/\*/g, "x"), [key.split("/").slice(1)]) !== null && lit !== "/api/inventory/x", true,
        `${key} now HAS a client (${lit}) — assert its request shape above and drop the pin (${why})`);
    }
    assert.ok(!built.has(asLiteral), `${key} now has a client — assert its shape above (${why})`);
  }
});

// ---------------------------------------------------------------------------
// Red proofs: each check fails on an injected violation
// ---------------------------------------------------------------------------

test("T-825 RED: the T-367 request itself — a bandless GET /api/timeline — fails the shape check", () => {
  // The bug that started this rule: a well-formed request for the wrong thing. The server answered,
  // the canvas drew nothing, every suite stayed green.
  assert.throws(() => requireShape({ method: "GET", path: timelineRequest(null, 256, 128) }, declared),
    /missing the required f_lo/);
  requireShape({ method: "GET", path: timelineRequest(BAND, 256, 128) }, declared); // the fixed shape passes
});

test("T-825 RED: an undeclared route, a NaN parameter and an inverted window each go red", () => {
  assert.throws(() => requireShape({ method: "GET", path: "/api/secret?f_lo=1&f_hi=2" }, declared), /does not declare/);
  assert.throws(() => requireShape({ method: "GET", path: `/api/priors?f_lo=1&f_hi=NaN&t0=${T0}&t1=${T1}` }, declared), /NaN/);
  assert.throws(() => requireShape({ method: "GET", path: `/api/priors?f_lo=9&f_hi=2&t0=${T0}&t1=${T1}` }, declared), /f_lo >= f_hi/);
  assert.throws(() => requireShape({ method: "GET", path: "/api/priors?f_lo=1&f_hi=2&t0=9&t1=2" }, declared), /t0 >= t1/);
});

test("T-825 RED: a §11 row nobody asserts, and a module asking for an undeclared route", () => {
  const fixture = `## 11. Panel -> state -> route\n| P | T | s | \`GET /api/nobody-asserts-this\` | - |\n## Sources\n`;
  const fake = declaredRoutes(fixture);
  assert.deepEqual(fake.map(patternKey), ["/api/nobody-asserts-this"]);
  // The coverage check, run over the fixture: nothing drives it, nothing names it, nothing pins it.
  const gaps = fake.map(patternKey).filter((k) => !(k in ASSERTED_ELSEWHERE) && !(k in NO_CLIENT_YET));
  assert.deepEqual(gaps, ["/api/nobody-asserts-this"], "a new §11 row must be red until it is asserted");
  // And the module scan, over an injected source: an undeclared ask is caught wherever it is written.
  const injected = 'const p = `/api/whatever/${id}/pretty-please`;';
  assert.deepEqual(routeLiteralsIn(injected), ["/api/whatever/*/pretty-please"]);
  assert.equal(matchDeclared("/api/whatever/x/pretty-please", declared), null);
  // A comment is not code (the scan strips comments, so prose about a route is not a violation).
  assert.deepEqual(routeLiteralsIn("// we no longer call /api/gone/away\n"), []);
});
