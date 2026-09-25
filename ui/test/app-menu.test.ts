// T-192 (docs/14-ui-rewrite.md "Added scope from docs/15 §7", ADR-0013 §4.5, §8): the right-click/
// long-press context menu's pure model (open/close, keyboard, viewport clamping) and the item
// builders' action wiring against a fake client/store. No DOM under node:test (ui/test/app-shell
// .test.ts's header note): `menu.ts`'s DOM component is a thin sync layer over `model.ts`'s
// `MenuState` reducer, exercised here directly; Listen (not yet on air) is never invoked because it
// creates a real `AudioContext` (see app-dock.test.ts's header note — canvas/WebGL/audio need a
// browser), so those cases assert on the built item's label/hint only, same as that file's pattern.
import { test } from "node:test";
import assert from "node:assert/strict";
import { ControlError } from "../src/controls/client";
import { createStore } from "../src/app/store";
import { initialState, type AppState, type OutputEntry } from "../src/app/state";
import type { AppContext } from "../src/app/context";
import {
  CLOSED_MENU, activeItem, clampMenuPosition, closeMenu, firstEnabledIndex, menuKeyDown, moveActiveIndex, openMenu, type MenuItem,
} from "../src/app/menu/model";
import { movedPastTolerance } from "../src/app/menu/trigger";
import { analyzeTarget, selectionMenuItems, signalMenuItems } from "../src/app/menu/actions";
import type { Row } from "../src/app/explore/inventory";
import type { Selection } from "../src/app/explore/selections";
import { setInventoryRows } from "../src/app/explore/slice";

// ---- fixtures ----

function makeRow(over: Partial<Row> = {}): Row {
  return {
    id: "e1", state: "confirmed",
    f_center_hz: 101_300_000, bandwidth_hz: 183_000, f_lo_hz: 101_208_500, f_hi_hz: 101_391_500,
    first_seen_s: 1_789_300_000, last_seen_s: 1_789_300_900, count: 42,
    known_status: "known", status: null, tags: [], family: "wfm-broadcast",
    identity_scheme: null, identity_class: null, withheld: false,
    recurrence: { occurrences: 12, appearances: 3, span_s: 3600, on_air_s: 900, duty_cycle: 0.25, recent: [] },
    classification: null, explanations: [], refined: null, cluster_id: null, cluster_group: null,
    ...over,
  };
}

function makeSelection(over: Partial<Selection> = {}): Selection {
  return { id: "s1", name: "Around 101.3", f_lo: 101_180_000, f_hi: 101_620_000, tags: [], links: [], created: 1, updated: 1, ...over };
}

function item(id: string, disabled = false): MenuItem {
  return { id, label: id, disabled, onSelect: () => {} };
}

interface Handlers {
  get?: (path: string) => unknown;
  post?: (path: string, body?: unknown) => unknown;
  del?: (path: string) => unknown;
}

function fakeCtx(handlers: Handlers = {}): AppContext {
  const store = createStore(initialState());
  const client = {
    get: async <T>(path: string): Promise<T> => { if (!handlers.get) throw new Error(`unexpected GET ${path}`); return handlers.get(path) as T; },
    post: async <T>(path: string, body?: unknown): Promise<T> => { if (!handlers.post) throw new Error(`unexpected POST ${path}`); return handlers.post(path, body) as T; },
    put: async () => { throw new Error("unexpected PUT"); },
    del: async <T>(path: string): Promise<T> => { if (!handlers.del) throw new Error(`unexpected DELETE ${path}`); return handlers.del(path) as T; },
  } as unknown as AppContext["client"];
  return { store, client, token: "t" };
}

/** Seeds a live audio output for `emitterId`, so `startListen`/`stopOutput` never reach
 * `AudioSession` (dock/api.ts short-circuits on an existing entry; app-dock.test.ts uses the same
 * trick). */
function seedListening(ctx: AppContext, emitterId: string): void {
  const entry: OutputEntry = {
    id: `listen-${emitterId}`, kind: "audio", label: "", sub: "", state: "live", tcpTarget: null,
    muted: false, levelDbfs: null, recordsPerS: null, emitterId, pipelineId: null, outputId: null, message: null,
  };
  ctx.store.set((s: AppState) => ({ outputs: [...s.outputs, entry] }));
}

// ---- model: viewport clamping ----

test("clampMenuPosition: keeps the menu inside the viewport, pinned to the margin at the edges", () => {
  // Fits with room to spare: anchored exactly at the pointer.
  assert.deepEqual(clampMenuPosition({ x: 50, y: 50 }, { w: 180, h: 200 }, { w: 800, h: 600 }), { x: 50, y: 50 });
  // Off the right/bottom edge: pulled back to stay fully visible.
  assert.deepEqual(clampMenuPosition({ x: 780, y: 590 }, { w: 200, h: 240 }, { w: 800, h: 600 }), { x: 596, y: 356 });
  // Negative anchor (a bogus event position): pinned to the top-left margin, not negative.
  assert.deepEqual(clampMenuPosition({ x: -20, y: -20 }, { w: 180, h: 200 }, { w: 800, h: 600 }), { x: 4, y: 4 });
  // A 400px-wide viewport (phone width, T-192's "stays inside the viewport at 400px" rule): a
  // right-click near the edge still keeps the whole popup on screen.
  const pos = clampMenuPosition({ x: 390, y: 300 }, { w: 220, h: 260 }, { w: 400, h: 700 });
  assert.ok(pos.x + 220 <= 400 && pos.x >= 4);
  // A menu wider than the viewport itself is pinned to the margin rather than pushed negative.
  assert.deepEqual(clampMenuPosition({ x: 10, y: 10 }, { w: 500, h: 100 }, { w: 400, h: 700 }, 4), { x: 4, y: 10 });
});

// ---- model: keyboard navigation ----

test("moveActiveIndex: Down/Up/Home/End wrap and skip disabled rows", () => {
  const items = [item("a"), item("b", true), item("c"), item("d", true)];
  assert.equal(moveActiveIndex(-1, "ArrowDown", items), 0);
  assert.equal(moveActiveIndex(0, "ArrowDown", items), 2, "b is disabled, skipped");
  assert.equal(moveActiveIndex(2, "ArrowDown", items), 0, "wraps past d (disabled) back to a");
  assert.equal(moveActiveIndex(0, "ArrowUp", items), 2, "wraps backward, skipping d");
  assert.equal(moveActiveIndex(-1, "Home", items), 0);
  assert.equal(moveActiveIndex(-1, "End", items), 2, "d is disabled, so End lands on c");
  assert.equal(moveActiveIndex(1, "PageDown", items), 1, "an unhandled key leaves the index alone");
});

test("moveActiveIndex: empty or all-disabled lists always yield -1", () => {
  assert.equal(moveActiveIndex(0, "ArrowDown", []), -1);
  assert.equal(moveActiveIndex(0, "ArrowDown", [item("a", true), item("b", true)]), 0, "no enabled row to land on");
});

test("firstEnabledIndex: first non-disabled row, -1 when none", () => {
  assert.equal(firstEnabledIndex([item("a", true), item("b"), item("c")]), 1);
  assert.equal(firstEnabledIndex([]), -1);
  assert.equal(firstEnabledIndex([item("a", true)]), -1);
});

// ---- model: open/close state machine ----

test("openMenu highlights the first enabled item; opening with no items stays closed", () => {
  const s = openMenu([item("a", true), item("b")]);
  assert.equal(s.open, true);
  assert.equal(s.active, 1);
  assert.deepEqual(openMenu([]), CLOSED_MENU);
});

test("menuKeyDown: Escape closes, arrows move the highlight, a closed menu ignores every key", () => {
  const open = openMenu([item("a"), item("b")]);
  assert.equal(menuKeyDown(open, "Escape"), CLOSED_MENU);
  const moved = menuKeyDown(open, "ArrowDown");
  assert.equal(moved.open, true);
  assert.equal(moved.active, 1);
  assert.equal(menuKeyDown(open, "KeyQ"), open, "an unhandled key is a no-op (same reference)");
  assert.equal(menuKeyDown(closeMenu(), "ArrowDown"), CLOSED_MENU, "nothing to move on a closed menu");
});

test("activeItem: the highlighted row, or null when closed / nothing highlighted / disabled", () => {
  const a = item("a"), b = item("b", true);
  assert.equal(activeItem(openMenu([a, b])), a);
  assert.equal(activeItem({ open: true, items: [a, b], active: 1 }), null, "the highlighted row (b) is disabled");
  assert.equal(activeItem(CLOSED_MENU), null);
  assert.equal(activeItem({ open: true, items: [a, b], active: -1 }), null);
});

// ---- trigger: long-press cancel-on-move ----

test("movedPastTolerance: long-press is cancelled once the touch drifts past the tolerance", () => {
  assert.equal(movedPastTolerance(100, 100, 104, 103), false, "within tolerance: still a long-press");
  assert.equal(movedPastTolerance(100, 100, 100, 100), false);
  assert.equal(movedPastTolerance(100, 100, 130, 100), true, "past the default 10px tolerance");
  assert.equal(movedPastTolerance(100, 100, 106, 100, 20), false, "a wider tolerance overrides the default");
});

// ---- signal menu items ----

test("signalMenuItems: Listen toggles label/hint with whether the emitter is already on air", () => {
  const ctx = fakeCtx();
  const idle = signalMenuItems(ctx, makeRow());
  assert.deepEqual(idle.map((i) => i.id), ["listen", "decode", "analyze", "export", "stream", "delete", "adjust-band"]);
  assert.equal(idle[0].label, "Listen");
  assert.equal(idle[0].hint, "adds to Outputs");

  seedListening(ctx, "e1");
  const onAir = signalMenuItems(ctx, makeRow());
  assert.equal(onAir[0].label, "Stop listening");
  onAir[0].onSelect(); // safe: stopOutput no-ops on AudioSession for an id it never opened
  assert.equal(ctx.store.get().outputs.length, 0, "Stop listening removes the dock entry");
});

test("signalMenuItems: Promote appears only for candidates; Delete's wording follows state", () => {
  const confirmed = signalMenuItems(fakeCtx(), makeRow({ state: "confirmed" }));
  assert.ok(!confirmed.some((i) => i.id === "promote"));
  assert.equal(confirmed.find((i) => i.id === "delete")!.label, "Delete from inventory");

  const candidate = signalMenuItems(fakeCtx(), makeRow({ state: "candidate" }));
  assert.ok(candidate.some((i) => i.id === "promote"));
  assert.equal(candidate.find((i) => i.id === "delete")!.label, "Delete");
});

// T-458 made this item ARM the next region stroke rather than describe a gesture. T-456 gave the
// drag to panning and the cutover retired the box's edge handles, so the old hint — "drag the
// yellow box's left/right edges on the live view" — named two things that no longer existed, and
// T-193's override was left with a reader and no setter at all. The assertion that the item *arms*
// is the one that keeps that from happening again: a menu item that only toasts is how the override
// came to be unreachable.
test("signalMenuItems: Adjust band ARMS the next region stroke for that row (T-193/T-458)", () => {
  const ctx = fakeCtx();
  const confirmed = signalMenuItems(ctx, makeRow({ id: "e9", state: "confirmed" })).find((i) => i.id === "adjust-band")!;
  assert.equal(confirmed.hint, "then shift-drag the new band");
  assert.equal(ctx.store.get().bandEdit, null, "nothing is armed until the item is chosen");
  confirmed.onSelect();
  assert.deepEqual(ctx.store.get().focus, { kind: "signal", id: "e9" });
  assert.equal(ctx.store.get().bandEdit, "e9", "the item must arm the stroke, not merely describe one");
  assert.match(ctx.store.get().toast.text, /^Adjust band: shift-drag/);

  // A candidate has no band to override, so it arms nothing — the refusal is a state, not just a
  // sentence: arming a row whose override the backend would reject is dead state with extra steps.
  const candidate = signalMenuItems(fakeCtx(), makeRow({ id: "e9", state: "candidate" })).find((i) => i.id === "adjust-band")!;
  assert.equal(candidate.hint, "promote it first");
  const ctx2 = fakeCtx();
  signalMenuItems(ctx2, makeRow({ id: "e9", state: "candidate" })).find((i) => i.id === "adjust-band")!.onSelect();
  assert.match(ctx2.store.get().toast.text, /^Adjust band: promote/);
  assert.equal(ctx2.store.get().bandEdit, null, "a candidate must not arm a band override");
});

test("signalMenuItems: Reset band appears only when a user band is set, and DELETEs it via the band route", async () => {
  const noOverride = signalMenuItems(fakeCtx(), makeRow({ id: "e1" }));
  assert.ok(!noOverride.some((i) => i.id === "reset-band"));

  const ub = { f_lo: 1, f_hi: 2, set_at: 1, actor: "fp", reason: null, reason_withheld: false };
  const calls: string[] = [];
  const ctx = fakeCtx({ del: (path) => { calls.push(`DELETE ${path}`); return { cleared: true, entry: makeRow({ id: "e1", user_band: null }) }; } });
  const reset = signalMenuItems(ctx, makeRow({ id: "e1", user_band: ub })).find((i) => i.id === "reset-band")!;
  assert.equal(reset.label, "Reset band");
  reset.onSelect();
  await new Promise((r) => setTimeout(r, 0));
  assert.deepEqual(calls, ["DELETE /api/inventory/e1/band"]);
  assert.equal(ctx.store.get().toast.text, "Band reset to the measured extent");
});

test("signalMenuItems: Reset band reports the server's refusal, without a thrown exception", async () => {
  const ub = { f_lo: 1, f_hi: 2, set_at: 1, actor: "fp", reason: null, reason_withheld: false };
  const ctx = fakeCtx({ del: () => { throw new ControlError(404, "not_found", "unknown id"); } });
  signalMenuItems(ctx, makeRow({ id: "e1", user_band: ub })).find((i) => i.id === "reset-band")!.onSelect();
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(ctx.store.get().toast.text, "Reset band: unknown id (not_found)");
});

test("signalMenuItems: Delete DELETEs the entry, then reloads both inventory tabs", async () => {
  const calls: string[] = [];
  const ctx = fakeCtx({
    del: (path) => { calls.push(`DELETE ${path}`); return {}; },
    get: (path) => { calls.push(`GET ${path}`); return { entries: [], next_cursor: null }; },
  });
  // T-379: a reload needs a live edge on the **capture** clock to name its window; without one it
  // declines to ask rather than inventing a browser-clock window that would select nothing.
  ctx.store.set((s) => ({ live: { ...s.live, rowRateHz: 25, edgeTS: 1_789_297_847 } }));
  signalMenuItems(ctx, makeRow({ id: "e7" })).find((i) => i.id === "delete")!.onSelect();
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(calls[0], "DELETE /api/inventory/e7");
  // T-260 (ADR-0017 §2.2): the Candidate reload is scoped to the waterfall's time window and the
  // Confirmed reload deliberately is not, so a confirmed station that has gone quiet is still
  // listed after the delete. The asymmetry lives in the request, which is what this asserts.
  // T-389: Confirmed still carries no `t0`/`t1` — but it does name the capture clock's live edge
  // as `at`, which scopes its rows' `presence` without selecting any of them.
  // T-587: `relations=all` now rides every Explore query (see app-explore.test.ts) — an artefact
  // the backend has explained must reach the list, not be hidden by the `shown` default.
  assert.ok(calls.includes("GET /api/inventory?state=confirmed&at=1789297847&relations=all&limit=200"), calls.join(", "));
  assert.ok(
    calls.some((c) => /^GET \/api\/inventory\?state=candidate&t0=[\d.]+&t1=[\d.]+&relations=all&limit=200$/.test(c)),
    calls.join(", "),
  );
});

test("signalMenuItems: Delete removes the row from the store immediately, before the DELETE resolves (T-187)", async () => {
  let resolveDel: () => void = () => {};
  const ctx = fakeCtx({
    del: () => new Promise((res) => { resolveDel = () => res({}); }),
    get: () => ({ entries: [], next_cursor: null }),
  });
  const row = makeRow({ id: "e7" });
  ctx.store.set(setInventoryRows({ e7: row }, 1));
  signalMenuItems(ctx, row).find((i) => i.id === "delete")!.onSelect();
  assert.ok(!("e7" in ctx.store.get().inventory.rows), "gone from the store before the server replied");
  resolveDel();
  await new Promise((r) => setTimeout(r, 0));
  assert.ok(!("e7" in ctx.store.get().inventory.rows), "stays gone once the server confirms");
});

test("signalMenuItems: a refused Delete puts the row back and toasts the reason (T-187)", async () => {
  const ctx = fakeCtx({ del: () => { throw new ControlError(404, "not_found", "no such inventory entry"); } });
  const row = makeRow({ id: "e7" });
  ctx.store.set(setInventoryRows({ e7: row }, 1));
  signalMenuItems(ctx, row).find((i) => i.id === "delete")!.onSelect();
  assert.ok(!("e7" in ctx.store.get().inventory.rows), "optimistically removed first");
  await new Promise((r) => setTimeout(r, 0));
  assert.deepEqual(ctx.store.get().inventory.rows.e7, row, "put back after the refusal");
  assert.match(ctx.store.get().toast.text, /delete e7/);
});

// ---- selection menu items ----

test("selectionMenuItems: Listen to all is disabled with nothing inside", () => {
  const items = selectionMenuItems(fakeCtx(), makeSelection(), []);
  assert.equal(items.find((i) => i.id === "listen-all")!.disabled, true);
  assert.equal(items.find((i) => i.id === "listen-all")!.hint, "0 streams at once");
});

test("selectionMenuItems: Listen to all starts one Listen per row inside (already on air, so no AudioContext)", () => {
  const ctx = fakeCtx();
  const rows = [makeRow({ id: "in1" }), makeRow({ id: "in2" })];
  for (const r of rows) seedListening(ctx, r.id);
  const before = ctx.store.get().outputs.length;
  const items = selectionMenuItems(ctx, makeSelection(), rows);
  const listenAll = items.find((i) => i.id === "listen-all")!;
  assert.equal(listenAll.disabled, false);
  listenAll.onSelect();
  assert.equal(ctx.store.get().outputs.length, before, "both emitters already had a dock entry: no duplicates");
});

test("selectionMenuItems: Delete is wired as a danger action, labelled to keep the detections", () => {
  // Not invoked: `selectionStoreFor` (the real "Delete selection" target) starts a page-lifetime
  // 15s flush interval (explore/selections.ts), which never exits a node:test process — the same
  // reason app-explore.test.ts/selections.test.ts exercise `SelectionStore` directly instead of
  // through `selectionStoreFor`. The item's shape is what T-192 owns here; the store itself is
  // T-151's, already tested in selections.test.ts.
  const it = selectionMenuItems(fakeCtx(), makeSelection({ id: "sel-del" }), []).find((i) => i.id === "delete")!;
  assert.equal(it.label, "Delete selection");
  assert.equal(it.hint, "detections kept");
  assert.equal(it.danger, true);
});

// ---- Analyze (T-190 stub landing later) ----

test("analyzeTarget: 501 not_implemented is a non-error 'not implemented yet' notice", async () => {
  const client = { post: async () => { throw new ControlError(501, "not_implemented", "no engine yet"); } };
  const res = await analyzeTarget(client, { kind: "emitter", id: "e1" });
  assert.deepEqual(res, { ok: false, notImplemented: true, message: "Analyze: not implemented yet" });
});

test("analyzeTarget: a 404 route (an older server) is also 'not implemented yet', not an error", async () => {
  const client = { post: async () => { throw new ControlError(404, "not_found", "no such route"); } };
  const res = await analyzeTarget(client, { kind: "selection", id: "s1" });
  assert.deepEqual(res, { ok: false, notImplemented: true, message: "Analyze: not implemented yet" });
});

test("analyzeTarget: any other failure is a real error, not the not-implemented notice", async () => {
  const client = { post: async () => { throw new ControlError(500, "internal", "boom"); } };
  const res = await analyzeTarget(client, { kind: "emitter", id: "e1" });
  assert.equal(res.ok, false);
  assert.equal((res as { notImplemented: boolean }).notImplemented, false);
  assert.match(res.message, /^Analyze: /);
  assert.doesNotMatch(res.message, /not implemented yet/);
});

test("analyzeTarget: success posts the right body shape for each target kind", async () => {
  let seen: { path: string; body: unknown } | null = null;
  const client = { post: async (path: string, body?: unknown) => { seen = { path, body }; return {}; } };
  const okEmitter = await analyzeTarget(client, { kind: "emitter", id: "e1" });
  assert.deepEqual(okEmitter, { ok: true, message: "Analyze: requested" });
  assert.deepEqual(seen, { path: "/api/analyze", body: { emitter_id: "e1" } });

  const okSelection = await analyzeTarget(client, { kind: "selection", id: "s1" });
  assert.deepEqual(okSelection, { ok: true, message: "Analyze: requested" });
  assert.deepEqual(seen, { path: "/api/analyze", body: { selection_id: "s1" } });
});

test("signal/selection Analyze menu items toast the analyzeTarget result", async () => {
  const ctx = fakeCtx({ post: () => { throw new ControlError(501, "not_implemented", "stub"); } });
  signalMenuItems(ctx, makeRow()).find((i) => i.id === "analyze")!.onSelect();
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(ctx.store.get().toast.text, "Analyze: not implemented yet");

  const ctx2 = fakeCtx({ post: () => ({}) });
  selectionMenuItems(ctx2, makeSelection(), []).find((i) => i.id === "analyze")!.onSelect();
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(ctx2.store.get().toast.text, "Analyze: requested");
});
