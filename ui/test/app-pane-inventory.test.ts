// T-1002 (MMAP split view): **detections are queried PER PANE.**
//
// The user's case: "look at one signal from the past and the current waterfall". Panes are
// independent in `surface/panes.ts`, but the inventory was not — the Candidate/Confirmed queries
// came from ONE window (`state.live.view` + `state.time`, the mirror of whichever pane was last
// pressed) and every pane drew the same `inventory.rows`. So freezing pane 1 on a past signal
// re-scoped pane 2's live boxes to pane 1's past window.
//
// What this file holds the client to, in the order the acceptance states it:
//   1. the REQUESTS it builds — one candidate + one confirmed query per pane, each carrying that
//      pane's own `(t, f)` window (assert the requests, not only the responses);
//   2. pane 2's rows do not change when pane 1 is scrubbed, or touched;
//   3. a pane draws its own rows and never another pane's;
//   4. the lists follow the active pane and NAME it.
// The browser half — the heading on screen, at 1280x800 and 400 px — is `ui/e2e/app-pane-inventory.e2e.mjs`.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import type { AppContext } from "../src/app/context";
import { loadInventoryRows, type Row } from "../src/app/explore/inventory";
import {
  listPaneNames, paneConfirmedFilters, paneSpecsKey, paneWindow, type PaneWindowSpec,
} from "../src/app/explore/pane-window";
import {
  paneRows, patchInventoryRow, removeInventoryRowLocal, setInventoryPanes, setInventoryRows,
  setPaneInventory, type ActiveInventoryPane,
} from "../src/app/explore/slice";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";

/** The capture clock's live edge in these fixtures — days from wall time on purpose (T-379). */
const EDGE = 1_789_540_000;
/** Where pane 1 is frozen: a past signal, well before the live edge. */
const PAST = EDGE - 3600;

const row = (id: string, state: "candidate" | "confirmed" = "candidate"): Row =>
  ({ id, state, f_center_hz: 101.3e6, bandwidth_hz: 180e3, explanations: [], count: 1 }) as unknown as Row;

const spec = (over: Partial<PaneWindowSpec> = {}): PaneWindowSpec => ({
  id: "p1", n: 1, loHz: 99.6e6, hiHz: 102e6, live: true, tS: null, spanS: 20, ...over,
});

const named = (id: string, n: number, count: number): ActiveInventoryPane =>
  ({ id, n, count, label: `pane ${n} of ${count}` });

/**
 * A store and a client that answers **by window**: each pane gets rows that belong to the window it
 * asked about, so a pane drawing another pane's answer is visible as the wrong rows rather than as
 * a coincidence. Records every path, so the requests themselves can be asserted.
 */
function paneCtx() {
  const paths: string[] = [];
  const store = createStore(initialState());
  const client = {
    get: async <T>(path: string): Promise<T> => {
      paths.push(path);
      if (path.startsWith("/api/coverage")) return { any: { cells: [{ state: "observed" }] } } as T;
      const q = new URLSearchParams(path.slice(path.indexOf("?") + 1));
      const tab = q.get("state") as "confirmed" | "candidate";
      // Which window is being asked about decides what comes back: the past window holds `old`,
      // the live one holds `now`.
      const past = Number(q.get("t1") ?? q.get("at")) < EDGE - 60;
      const id = `${past ? "old" : "now"}-${tab}`;
      return { entries: [row(id, tab)], next_cursor: null } as T;
    },
  } as unknown as AppContext["client"];
  const ctx: AppContext = { store, client, token: "t" };
  const queries = (tab: string) =>
    paths.filter((p) => p.includes(`state=${tab}`)).map((p) => new URLSearchParams(p.slice(p.indexOf("?") + 1)));
  /** The capture clock's live edge, the way the spectrum stream reports it. */
  store.set((s) => ({ live: { ...s.live, rowRateHz: 25, edgeTS: EDGE, view: { loHz: 99.6e6, hiHz: 102e6 } } }));
  return { ctx, store, paths, queries };
}

/** Two panes: pane 1 frozen on a past signal, pane 2 following the live edge. */
const SPLIT: PaneWindowSpec[] = [
  spec({ id: "p1", n: 1, live: false, tS: PAST, spanS: 20 }),
  spec({ id: "p2", n: 2, live: true, tS: null, spanS: 40, loHz: 430e6, hiHz: 440e6 }),
];

// ---- 1. the requests the client builds, per pane -------------------------------------------

test("THE ACCEPTANCE: each pane's Candidate query carries ITS OWN window, and each Confirmed query its own instant", async () => {
  const { ctx, store, queries } = paneCtx();
  store.set(setInventoryPanes(SPLIT, named("p2", 2, 2)));
  await loadInventoryRows(ctx, () => {});

  const cand = queries("candidate");
  assert.equal(cand.length, 2, "one candidate query per pane on screen, never one for the pair");
  const [c1, c2] = cand.sort((a, b) => Number(a.get("t1")) - Number(b.get("t1")));
  // Pane 1: frozen on the past signal. Its window is its own frozen instant, not the live edge.
  assert.deepEqual(
    { t0: Number(c1.get("t0")), t1: Number(c1.get("t1")), f_lo: Number(c1.get("f_lo")), f_hi: Number(c1.get("f_hi")) },
    { t0: PAST - 20, t1: PAST, f_lo: 99.6e6, f_hi: 102e6 },
    "pane 1's candidate window is the past window IT is showing",
  );
  // Pane 2: following. Its window ends at the live edge and is its own span and its own band —
  // a different frequency range entirely, which the shared query could not express at all.
  assert.deepEqual(
    { t0: Number(c2.get("t0")), t1: Number(c2.get("t1")), f_lo: Number(c2.get("f_lo")), f_hi: Number(c2.get("f_hi")) },
    { t0: EDGE - 40, t1: EDGE, f_lo: 430e6, f_hi: 440e6 },
    "pane 2's candidate window is the live edge and pane 2's own span",
  );

  const conf = queries("confirmed").sort((a, b) => Number(a.get("at")) - Number(b.get("at")));
  assert.equal(conf.length, 2, "one confirmed query per pane too");
  assert.equal(Number(conf[0].get("at")), PAST, "pane 1's Confirmed list reads the liveness of the instant IT shows");
  assert.equal(Number(conf[1].get("at")), EDGE, "pane 2's reads the live edge's");
  for (const q of conf) {
    // ADR-0017 §2.2, unchanged by this ticket: a window selects rows, so Confirmed is never
    // time-filtered — `at` scopes the presence projection and selects nothing.
    assert.equal(q.get("t0"), null);
    assert.equal(q.get("t1"), null);
    assert.equal(q.get("relations"), "all", "T-587: artefacts reach the screen labelled, per pane");
  }
});

test("with no pane registry published there is exactly one window, asked about once (the pre-split behaviour)", async () => {
  const { ctx, queries } = paneCtx();
  await loadInventoryRows(ctx, () => {});
  assert.equal(queries("candidate").length, 1);
  assert.equal(queries("confirmed").length, 1);
  assert.equal(Number(queries("candidate")[0].get("t1")), EDGE);
});

test("a pane whose window is not known yet is not asked about on a clock of the client's own (T-379)", async () => {
  const { ctx, store, queries } = paneCtx();
  // No live edge reported: a FOLLOWING pane's window is unknown. A frozen pane's is not — it is a
  // property of the pane — so pane 1 is still asked about and pane 2 is not.
  store.set((s) => ({ live: { ...s.live, edgeTS: null } }));
  store.set(setInventoryPanes(SPLIT, named("p1", 1, 2)));
  await loadInventoryRows(ctx, () => {});
  const cand = queries("candidate");
  assert.equal(cand.length, 1, "the pane with no knowable window is not asked about at all");
  assert.equal(Number(cand[0].get("t1")), PAST);
  assert.equal(store.get().inventory.panes.p2.window, null, "and it says its window is unknown, not empty");
  assert.equal(store.get().inventory.panes.p2.rows.now, undefined);
});

// ---- 2. pane 2 does not move when pane 1 is touched ----------------------------------------

test("THE ACCEPTANCE: pane 2's rows do not change when pane 1 is scrubbed", async () => {
  const { ctx, store, queries } = paneCtx();
  store.set(setInventoryPanes(SPLIT, named("p2", 2, 2)));
  await loadInventoryRows(ctx, () => {});
  const before = store.get().inventory.panes.p2;
  assert.deepEqual(Object.keys(before.rows).sort(), ["now-candidate", "now-confirmed"], "pane 2 holds the live window's rows");
  assert.deepEqual(Object.keys(store.get().inventory.panes.p1.rows).sort(), ["old-candidate", "old-confirmed"]);

  // Pane 1 is scrubbed further back, and becomes the pane the chrome acts on. Pane 2 is not touched.
  store.set(setInventoryPanes(
    [spec({ id: "p1", n: 1, live: false, tS: PAST - 1800, spanS: 20 }), SPLIT[1]],
    named("p1", 1, 2),
  ));
  await loadInventoryRows(ctx, () => {});
  const after = store.get().inventory.panes.p2;
  // This is the defect, stated as a comparison: before the ticket, pane 1's scrub moved the ONE
  // window every pane drew, so these rows became the past window's and this pane's box went with
  // them. The rows and the window are of the live edge, unchanged, whatever pane 1 did.
  assert.deepEqual(after.rows, before.rows, "pane 2's rows are the live window's still");
  assert.deepEqual(after.window, before.window, "…and so is the window they are of");
  assert.equal(after.window?.t1, EDGE);
  assert.deepEqual(Object.keys(store.get().inventory.panes.p1.rows).sort(), ["old-candidate", "old-confirmed"],
    "while pane 1 has the rows of where it was scrubbed to");
  // And the requests say the same: pane 2 asked about the live edge both times, pane 1 about two
  // different past instants.
  const ends = queries("candidate").map((q) => Number(q.get("t1")));
  assert.deepEqual(ends.filter((t) => t === EDGE).length, 2, "pane 2's query named the live edge on both polls");
  assert.deepEqual(ends.filter((t) => t !== EDGE).sort(), [PAST - 1800, PAST], "pane 1's named where pane 1 was");
});

test("touching pane 1 re-scopes THE LISTS to pane 1 and nothing else — no request, no other pane moved", () => {
  const { store } = paneCtx();
  store.set(setInventoryPanes(SPLIT, named("p2", 2, 2)));
  store.set(setPaneInventory("p1", { rows: { a: row("a") }, window: { t0: PAST - 20, t1: PAST, coverage: "observed" } }, 1));
  store.set(setPaneInventory("p2", { rows: { b: row("b") }, window: { t0: EDGE - 40, t1: EDGE, coverage: "observed" } }, 1));
  assert.deepEqual(Object.keys(store.get().inventory.rows), ["b"], "the lists show the active pane's rows");

  const p2 = store.get().inventory.panes.p2;
  store.set(setInventoryPanes(SPLIT, named("p1", 1, 2)));
  assert.deepEqual(Object.keys(store.get().inventory.rows), ["a"], "…and follow the pane that was pressed");
  assert.equal(store.get().inventory.window?.t1, PAST, "…including which window they are a list of");
  assert.equal(store.get().inventory.panes.p2, p2, "while pane 2's own answer is untouched");
});

test("a republish that moved nothing is not a store change at all (it runs on the frame hook)", () => {
  const { store } = paneCtx();
  store.set(setInventoryPanes(SPLIT, named("p2", 2, 2)));
  const inv = store.get().inventory;
  store.set(setInventoryPanes([spec({ id: "p1", n: 1, live: false, tS: PAST, spanS: 20 }), SPLIT[1]], named("p2", 2, 2)));
  assert.equal(store.get().inventory, inv, "same windows, same active pane: nothing written");
});

test("the first publish HANDS the unpaned window to the active pane instead of blanking the lists", () => {
  const { store } = paneCtx();
  store.set(setInventoryRows({ a: row("a") }, 1));
  assert.deepEqual(Object.keys(store.get().inventory.rows), ["a"]);
  store.set(setInventoryPanes([spec({ id: "p1", n: 1 })], named("p1", 1, 1)));
  assert.deepEqual(Object.keys(store.get().inventory.rows), ["a"], "the window was renamed, not lost");
  assert.deepEqual(Object.keys(store.get().inventory.panes), ["p1"], "and the unpaned entry is gone");
});

test("a closed pane's late answer is dropped, not resurrected", () => {
  const { store } = paneCtx();
  store.set(setInventoryPanes(SPLIT, named("p2", 2, 2)));
  store.set(setInventoryPanes([SPLIT[1]], named("p2", 1, 1)));
  store.set(setPaneInventory("p1", { rows: { a: row("a") }, window: { t0: 0, t1: 1, coverage: "observed" } }, 1));
  assert.deepEqual(Object.keys(store.get().inventory.panes), ["p2"]);
});

test("an optimistic delete and a presence patch reach EVERY pane showing the row", () => {
  const { store } = paneCtx();
  store.set(setInventoryPanes(SPLIT, named("p2", 2, 2)));
  const w = { t0: 0, t1: 1, coverage: "observed" as const };
  store.set(setPaneInventory("p1", { rows: { e1: row("e1"), e9: row("e9") }, window: w }, 1));
  store.set(setPaneInventory("p2", { rows: { e1: row("e1") }, window: w }, 1));
  // T-388's presence push extends a box's top; a row shown in two panes is one row.
  store.set(patchInventoryRow("e1", { count: 7 } as Partial<Row>));
  assert.equal(store.get().inventory.panes.p1.rows.e1.count, 7);
  assert.equal(store.get().inventory.panes.p2.rows.e1.count, 7);
  // T-187's optimistic delete: gone from both, and the pane that never had it is untouched.
  const p1 = store.get().inventory.panes.p1;
  store.set(removeInventoryRowLocal("e1"));
  assert.deepEqual(Object.keys(store.get().inventory.panes.p1.rows), ["e9"]);
  assert.deepEqual(Object.keys(store.get().inventory.panes.p2.rows), []);
  assert.notEqual(store.get().inventory.panes.p1, p1);
});

// ---- 3. a pane draws its own rows ----------------------------------------------------------

test("paneRows: a pane draws its own answer; an unanswered pane draws NOTHING, never the active pane's", () => {
  const { store } = paneCtx();
  // No registry: one window, and every pane draws it — the behaviour before this ticket, and what
  // a test that seeds rows directly still means.
  store.set(setInventoryRows({ a: row("a") }, 1));
  assert.deepEqual(Object.keys(paneRows(store.get().inventory, "p1")), ["a"]);

  store.set(setInventoryPanes(SPLIT, named("p2", 2, 2)));
  store.set(setPaneInventory("p2", { rows: { b: row("b") }, window: { t0: 0, t1: 1, coverage: "observed" } }, 1));
  assert.deepEqual(Object.keys(paneRows(store.get().inventory, "p2")), ["b"]);
  // Pane 1 has been published but not answered yet. Borrowing pane 2's rows here is exactly the
  // defect: a box is a claim about THIS pane's window.
  assert.deepEqual(Object.keys(paneRows(store.get().inventory, "p1")), []);
  assert.deepEqual(Object.keys(paneRows(store.get().inventory, "gone")), []);
});

test("STRUCTURAL: every mark the surface draws for a pane comes from that pane's own rows", () => {
  const src = readFileSync("src/app/centre/surface.ts", "utf8");
  // The four places a detection reaches the screen: the mark boxes, the `detections` overlay layer,
  // the artefact links between them, and the pin/feature layer over them. A fifth reader of
  // `s.inventory.rows` inside a per-pane callback would be the shared window growing back.
  assert.match(src, /paneMarkBoxes\(Object\.values\(paneRows\(s\.inventory, pane\.id\)\)/);
  assert.match(src, /const rows = Object\.values\(paneRows\(s\.inventory, pane\.id\)\);/);
  assert.match(src, /artifactLinks\(Object\.values\(paneRows\(store\.get\(\)\.inventory, pane\.id\)\)/);
  assert.match(src, /detectionPins\(Object\.values\(paneRows\(s\.inventory, v\.id\)\)\)/);
  // And the pane registry is published from the pane model, on the same hooks the mirror is: the
  // frame's view change, the poll, and the active-pane change (T-1000's own dispatch).
  assert.match(src, /store\.set\(setInventoryPanes\(specs, active\)\)/);
  assert.match(src, /publishPanes\(\);/);
});

// ---- 4. the lists name the pane ------------------------------------------------------------

test("THE ACCEPTANCE: the lists name the pane they show — and say nothing extra when there is one pane", () => {
  assert.equal(listPaneNames(null), null, "no registry: nothing to disambiguate");
  assert.equal(listPaneNames(named("p1", 1, 1)), null, "one pane: a badge on the only viewport is noise (§10.6 P1)");
  assert.deepEqual(listPaneNames(named("p1", 1, 2)), {
    heading: "· pane 1 of 2", confirmed: "Confirmed · pane 1 of 2", candidate: "Candidates · pane 1 of 2",
  });
  assert.deepEqual(listPaneNames(named("p2", 2, 3)).candidate, "Candidates · pane 2 of 3");
});

test("the sidebar renders that naming, and re-asks when ANY pane's window moves", () => {
  const idx = readFileSync("src/app/explore/index.ts", "utf8");
  assert.match(idx, /listPaneNames\(act\)/, "the heading and the tabs are named from one function");
  assert.match(idx, /paneName\.textContent = named\?\.heading/);
  assert.match(idx, /tabCandidate\.setAttribute\("aria-label", named\?\.candidate/);
  // T-1002: not `s.time` alone. The cursor is the ACTIVE pane's; a pane beside it moving, opening
  // or closing is a window the lists and that pane's boxes have no answer for yet.
  assert.match(idx, /paneSpecsKey\(Object\.values\(s\.inventory\.panes\)\.map\(\(p\) => p\.spec\), s\.inventory\.active\?\.id \?\? null\)/);
});

test("paneSpecsKey changes when a pane moves, opens, closes or becomes active — and not while one merely follows the live edge", () => {
  const k = (specs: PaneWindowSpec[], active: string) => paneSpecsKey(specs, active);
  const base = k(SPLIT, "p2");
  assert.equal(k([...SPLIT], "p2"), base, "a following pane re-publishing the same window is not a change");
  assert.notEqual(k(SPLIT, "p1"), base, "which pane the lists follow is");
  assert.notEqual(k([SPLIT[0]], "p1"), base, "a pane closing is");
  assert.notEqual(k([spec({ id: "p1", n: 1, live: false, tS: PAST - 10, spanS: 20 }), SPLIT[1]], "p2"), base, "a scrub is");
  assert.notEqual(k([SPLIT[1], { ...SPLIT[0], n: 2 }], "p2"), base, "and so is a pane changing position");
});

// ---- the arithmetic itself -----------------------------------------------------------------

test("paneWindow: a frozen pane's window is its own instant; a following one's ends at the live edge, or is UNKNOWN", () => {
  assert.deepEqual(paneWindow(spec({ live: false, tS: PAST, spanS: 20 }), EDGE), { t0: PAST - 20, t1: PAST },
    "a frozen pane ignores the edge entirely — that is what frozen means");
  assert.deepEqual(paneWindow(spec({ spanS: 40 }), EDGE), { t0: EDGE - 40, t1: EDGE });
  assert.equal(paneWindow(spec(), null), null, "no edge reported: unknown, never invented");
  assert.equal(paneWindow(spec({ spanS: 0 }), EDGE), null, "and a pane with no span on screen asks nothing");
});

test("paneConfirmedFilters names the pane's own instant, on the capture clock and never the browser's", () => {
  assert.equal(paneConfirmedFilters(spec({ live: false, tS: PAST }), EDGE).at, PAST);
  assert.equal(paneConfirmedFilters(spec(), EDGE).at, EDGE);
  assert.equal(paneConfirmedFilters(spec(), null).at, undefined, "no edge known: send nothing rather than a made-up now");
  assert.ok(Math.abs(PAST - Date.now() / 1000) > 1000, "the fixture is days from wall time, so a browser clock would show");
});
