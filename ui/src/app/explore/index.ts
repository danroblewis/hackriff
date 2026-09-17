// Explore sidebar (inventory, selections) and focus panel mounts (ADR-0013 §8, T-151). Renders
// only what the API served; explanations are always shown as ranked suggestions, never as truth
// (CLAUDE.md "Product vision" §4).
import { toast } from "../state";
import { sameCursor } from "../centre/review-render";
import type { AppContext, AreaMounts, MountFn } from "../context";
import { h } from "../dom";
import { bindContextTrigger, openSelectionMenu, openSignalMenu } from "../menu";
import { startPoll } from "../net";
import {
  apiErrorText, classificationDistribution, explanationWhy, fmtBandwidth, fmtMHz, rasterText,
  refinedNote, unknownScorePct, unknownScoreText,
} from "./format";
import { selectionSummary } from "./focus";
import {
  clusterChip, deleteEntry, emptyListText, loadInventoryRows, nextInventorySort, promoteEntry,
  recurrenceDots, rowChips, rowSeenText, sortInventoryRows, type Row,
} from "./inventory";
import { foundInside, selectionStoreFor, sortSelections, type Selection } from "./selections";
import {
  clusterLoadErrorText, clusterSummary, fetchCluster, fetchSignatureMatch, signatureMatchSummary,
  type Cluster, type SignatureMatch,
} from "./signature";
import {
  focusSelection, focusSignal, removeInventoryRowLocal, restoreInventoryRowLocal, setInventorySort,
  setInventoryTab, type InventorySortKey, type InventoryTab,
} from "./slice";

const SORT_LABEL: Record<InventorySortKey, string> = { freq: "Freq", last_seen: "Last seen", count: "Count", bandwidth: "Bandwidth" };
const SORT_KEYS: readonly InventorySortKey[] = ["freq", "last_seen", "count", "bandwidth"];

function isOn(ctx: AppContext, emitterId: string): string | null {
  const e = ctx.store.get().outputs.find((o) => o.kind === "audio" && o.emitterId === emitterId && (o.state === "live" || o.state === "opening"));
  return e ? e.id : null;
}

// ---- inventory (sidebar) ----

const mountInventory: MountFn = (el, ctx) => {
  const more: Record<InventoryTab, boolean> = { confirmed: false, candidate: false };
  const onMore = (tab: InventoryTab, m: boolean) => { more[tab] = m; };
  const reload = () => loadInventoryRows(ctx, onMore).catch((e) => ctx.store.set(toast(apiErrorText(e))));

  const heading = h("div", { class: "h" }, "Signal inventory ", h("em", {}, "this span"));
  const tabConfirmed = h("button", { class: "tab", role: "tab", type: "button", "data-tab": "confirmed", onclick: () => ctx.store.set(setInventoryTab("confirmed")) });
  const tabCandidate = h("button", { class: "tab", role: "tab", type: "button", "data-tab": "candidate", onclick: () => ctx.store.set(setInventoryTab("candidate")) });
  const tabs = h("div", { class: "tabs", role: "tablist" }, tabConfirmed, tabCandidate);
  const note = h("div", { class: "tab-note" });
  const sortRow = h("div", { class: "sort-row" }, h("span", { class: "hint" }, "Sort"),
    ...SORT_KEYS.map((k) => h("button", {
      class: "mini", type: "button", "data-sort": k,
      onclick: () => ctx.store.set(setInventorySort(nextInventorySort(ctx.store.get().inventory.sort, k))),
    }, SORT_LABEL[k])));
  const list = h("div", { class: "list", role: "list" });
  el.replaceChildren(h("div", { class: "side-head" }, heading, tabs, note, sortRow), list);
  bindContextTrigger(list, (x, y, target) => {
    const rowEl = target.closest<HTMLElement>(".row[data-id]");
    const r = rowEl?.dataset.id ? ctx.store.get().inventory.rows[rowEl.dataset.id] : undefined;
    if (r) openSignalMenu(ctx, r, x, y);
  });

  function actionButtons(r: Row): HTMLElement {
    const busy = { promoting: false, deleting: false };
    const promote = h("button", { class: "mini promote", type: "button", onclick: (e: Event) => {
      e.stopPropagation();
      if (busy.promoting) return;
      busy.promoting = true;
      void promoteEntry(ctx.client, r.id, reload).then((res) => { if (!res.ok) ctx.store.set(toast(`promote ${r.id}: ${res.message}`)); }).finally(() => { busy.promoting = false; });
    } }, "Promote");
    const del = h("button", { class: "mini del", type: "button", onclick: (e: Event) => {
      e.stopPropagation();
      if (busy.deleting) return;
      busy.deleting = true;
      // T-187: optimistic — the row leaves the list immediately (the candidate list fills up
      // fast, so discarding one must feel instant); a refused delete puts it right back.
      ctx.store.set(removeInventoryRowLocal(r.id));
      void deleteEntry(ctx.client, r.id, reload).then((res) => {
        if (!res.ok) {
          ctx.store.set(restoreInventoryRowLocal(r));
          ctx.store.set(toast(`delete ${r.id}: ${res.message}`));
        }
      }).finally(() => { busy.deleting = false; });
    } }, "Delete");
    return h("div", { class: "acts" }, r.state === "candidate" ? promote : null, del);
  }

  function renderRow(r: Row): HTMLElement {
    const focus = ctx.store.get().focus;
    const current = focus.kind === "signal" && focus.id === r.id;
    const cluster = clusterChip(r);
    const chips = [...rowChips(r), ...(cluster ? [cluster] : [])].map((c) => h("span", { class: `chip ${c.cls}` }, c.text));
    const dotsEl = r.state === "candidate" && recurrenceDots(r).length
      ? h("span", { class: "dots" }, ...recurrenceDots(r).map((v) => h("i", { style: `height:${2 + Math.round(v * 8)}px` })))
      : null;
    const unkPct = unknownScorePct(r.classification);
    return h("div", {
      class: "row", role: "listitem", tabindex: "0", "data-id": r.id,
      "aria-current": current ? "true" : undefined,
      onclick: () => ctx.store.set(focusSignal(r.id)),
      onkeydown: (e: Event) => { const ke = e as KeyboardEvent; if (ke.key === "Enter" || ke.key === " ") { ke.preventDefault(); ctx.store.set(focusSignal(r.id)); } },
    },
      h("div", { class: "f" }, fmtMHz(r.f_center_hz), h("small", {}, " MHz"),
        unkPct !== null ? h("span", { class: "unk-badge", title: "Unknown score" }, `${unkPct}% unk`) : null,
        isOn(ctx, r.id) ? h("span", { class: "on-air", title: "Streaming" }) : null),
      actionButtons(r),
      h("div", { class: "meta" }, ...chips, h("span", { class: "mono" }, fmtBandwidth(r.bandwidth_hz)), dotsEl, h("span", {}, rowSeenText(r))),
    );
  }

  function render() {
    const s = ctx.store.get();
    const rows = Object.values(s.inventory.rows);
    const confirmed = rows.filter((r) => r.state === "confirmed");
    const candidate = rows.filter((r) => r.state === "candidate");
    tabConfirmed.setAttribute("aria-selected", String(s.inventory.tab === "confirmed"));
    tabCandidate.setAttribute("aria-selected", String(s.inventory.tab === "candidate"));
    tabConfirmed.replaceChildren("Confirmed ", h("span", { class: "count" }, more.confirmed ? "500+" : String(confirmed.length)));
    tabCandidate.replaceChildren("Candidates ", h("span", { class: "count" }, more.candidate ? "500+" : String(candidate.length)));
    note.textContent = s.inventory.tab === "confirmed"
      ? "Strong, unambiguous signals are confirmed automatically."
      : "Seen more than once but not certain. Promote the ones worth keeping.";
    for (const b of sortRow.querySelectorAll<HTMLButtonElement>("button[data-sort]")) {
      b.setAttribute("aria-pressed", String(b.dataset.sort === s.inventory.sort.key));
    }
    const tabRows = s.inventory.tab === "confirmed" ? confirmed : candidate;
    const shown = sortInventoryRows(tabRows, s.inventory.sort.key, s.inventory.sort.dir);
    // T-379: an empty list says *which* emptiness it is. "Nothing here yet" read the same whether
    // the receiver had listened and heard nothing, had never looked, or had simply not been asked —
    // and the last of those was the bug the whole-UI window rule names.
    list.replaceChildren(...(shown.length ? shown.map(renderRow) : [h("div", { class: "empty" }, emptyListText(s.inventory))]));
  }

  ctx.store.select(
    (s) => [s.inventory, s.outputs, s.focus] as const,
    render,
    { immediate: true, eq: (a, b) => a[0] === b[0] && a[1] === b[1] && a[2] === b[2] },
  );
  // T-263 (ADR-0017 TM-7): scrubbing the capture timeline re-derives both lists at once, instead of
  // leaving the previous window's rows on screen for up to a poll interval beside a waterfall
  // already showing a different time. `loadInventoryRows` reads the cursor from the store at call
  // time, so the windows it sends follow the scrub; this only makes it happen immediately.
  ctx.store.select((s) => s.time, () => void reload(), { eq: sameCursor });
  void reload();
  startPoll(reload, 5000, (e) => ctx.store.set(toast(`inventory: ${apiErrorText(e)}`)));
};

// ---- selections (sidebar) ----

const mountSelections: MountFn = (el, ctx) => {
  selectionStoreFor(ctx); // creates the shared store and mirrors it into state.selections
  const head = h("div", { class: "side-sel-head" }, h("div", { class: "h" }, "Selections ", h("em", {}, "drag on the waterfall")));
  const list = h("div", { class: "sel-list" });
  el.replaceChildren(head, list);
  bindContextTrigger(list, (x, y, target) => {
    const selEl = target.closest<HTMLElement>(".sel[data-sel]");
    const sel = selEl?.dataset.sel ? ctx.store.get().selections.list.find((s) => s.id === selEl.dataset.sel) : undefined;
    if (sel) openSelectionMenu(ctx, sel, x, y);
  });

  function renderRow(s: Selection): HTMLElement {
    const focus = ctx.store.get().focus;
    const current = focus.kind === "selection" && focus.id === s.id;
    return h("div", {
      class: "sel", tabindex: "0", "data-sel": s.id, "aria-current": current ? "true" : undefined,
      onclick: () => ctx.store.set(focusSelection(s.id)),
      onkeydown: (e: Event) => { const ke = e as KeyboardEvent; if (ke.key === "Enter" || ke.key === " ") { ke.preventDefault(); ctx.store.set(focusSelection(s.id)); } },
    }, h("b", {}, s.name), h("span", { class: "mono" }, `${fmtMHz(s.f_lo, 2)}–${fmtMHz(s.f_hi, 2)}`));
  }

  function render() {
    const sels = sortSelections(ctx.store.get().selections.list);
    list.replaceChildren(...(sels.length ? sels.map(renderRow) : [h("div", { class: "empty" }, "Drag across the waterfall to mark a region.")]));
  }

  ctx.store.select((s) => [s.selections.list, s.focus] as const, render, { immediate: true, eq: (a, b) => a[0] === b[0] && a[1] === b[1] });
};

// ---- focus panel ----

function stateBadge(state: string): HTMLElement { return h("span", { class: `state ${state}` }, state); }

/** A loaded value, `"loading"` while its fetch is in flight, or `undefined` before it starts. */
type Loaded<T> = T | "loading" | undefined;

function renderSignalFocus(ctx: AppContext, r: Row, match: Loaded<SignatureMatch | null>, cluster: Loaded<Cluster | null>): HTMLElement {
  const chips = [stateBadge(r.state), ...rowChips(r).map((c) => h("span", { class: `chip ${c.cls}` }, c.text))];
  const flags = r.explanations[0]?.flags ?? [];
  const centerHz = r.refined?.center_hz ?? r.f_center_hz;
  const bwHz = r.refined?.bandwidth_hz ?? r.bandwidth_hz;

  const kv = h("dl", { class: "kv" },
    h("dt", {}, "Bandwidth"), h("dd", {}, fmtBandwidth(bwHz)),
    h("dt", {}, "Seen"), h("dd", {}, rowSeenText(r)),
    h("dt", {}, "Channel raster"), h("dd", { class: flags.includes("off-raster") ? "flag" : undefined }, rasterText(r.explanations[0]?.evidence ?? [])),
  );

  // T-207: unknown score shown prominently (unknown signals are the priority to surface —
  // CLAUDE.md "Product vision" §4), reading `classification.open_set_score` off the row, never
  // recomputed. "Not yet classified" (not "0% unknown") until a Classification exists.
  const unknownBanner = h("div", { class: "unk-score" },
    h("span", { class: "unk-label" }, "Unknown score"), h("span", { class: "unk-val" }, unknownScoreText(r.classification)));

  // T-207 classification distribution: `classification.top`, highest first, straight off the row
  // — a decision-tree posterior, not a re-ranking done here.
  const dist = classificationDistribution(r.classification);
  const distSection = dist.length
    ? h("div", {}, h("div", { class: "section-h" }, "Classification distribution"),
        h("div", { class: "dist" }, ...dist.map((d) => h("div", { class: "dist-row" },
          h("span", { class: "dist-label" }, d.label),
          h("span", { class: "dist-bar" }, h("i", { style: `width:${d.pct}%` })),
          h("span", { class: "dist-pct mono" }, `${d.pct}%`),
        ))))
    : null;

  const explanations = h("ol", { class: "expl" }, ...r.explanations.map((e) => h("li", {},
    h("span", { class: "rank" }, String(e.rank)),
    h("div", {}, h("b", {}, e.label), h("span", { class: "conf" }, e.score.toFixed(2))),
    h("div", { class: "why" }, explanationWhy(e.evidence)),
  )));

  // T-207: signature match (docs/api.md `GET /api/signatures/match`) — ranked evidence beside the
  // measurement, never identity. `undefined` while the fetch hasn't started/finished yet.
  const matchText = match === undefined ? "" : match === "loading" ? "Checking the catalogue…" : signatureMatchSummary(match);
  const matchSection = h("div", {}, h("div", { class: "section-h" }, "Signature match ", h("em", {}, "ranked evidence")),
    h("p", { class: "hint" }, matchText));

  // T-207: cluster ("I have seen this before", ADR-0016 §5) — a type above emitters, never an
  // identity. `cluster_id` is on the row already; the member/observation count needs its own
  // fetch.
  const clusterText = !r.cluster_id
    ? "Not part of a visible cluster yet."
    : cluster === undefined || cluster === "loading" ? "Checking clusters…"
    : cluster === null ? "Cluster lookup failed."
    : clusterSummary(cluster);
  const clusterSection = h("div", {}, h("div", { class: "section-h" }, "Cluster ", h("em", {}, "the same thing seen before")),
    h("p", { class: "hint" }, clusterText));

  // GAP 3: only the identity (not the latest decoded fields, e.g. RDS PS/PTY) is on the row, so
  // this shows only what's actually served — no invented decode summary.
  const identityBox = r.identity_scheme
    ? h("div", { class: "decode" },
        h("div", { class: "section-h", style: "margin-bottom:6px" }, "Identity"),
        h("dl", { class: "kv" }, h("dt", {}, r.identity_scheme), h("dd", {}, r.identity_value ?? (r.withheld ? "withheld" : "—"))))
    : null;

  // Actions (Listen, Decode, Analyze, Record/Export, Stream out, Promote, Delete, Adjust band)
  // moved to the right-click/long-press context menu (T-192); this panel keeps only measurements
  // and explanations, freeing the space for per-signal output panels (T-195).
  return h("div", {},
    h("div", {}, h("div", { class: "eyebrow" }, ...chips), h("div", { class: "bigf" }, fmtMHz(centerHz), h("small", {}, " MHz")), h("div", { class: "sub" }, refinedNote(r.refined))),
    unknownBanner,
    kv,
    distSection,
    h("div", {}, h("div", { class: "section-h" }, "Possible explanations ", h("em", {}, "ranked suggestions")), explanations),
    matchSection,
    clusterSection,
    h("div", { class: "hint" }, "Right-click or long-press the signal for actions: Listen, Decode, Analyze, Export clip, Stream out, Promote, Delete, Adjust band."),
    identityBox,
  );
}

function renderSelectionFocus(ctx: AppContext, s: Selection): HTMLElement {
  const rows = foundInside(s, Object.values(ctx.store.get().inventory.rows));
  const inside = h("div", { class: "list", style: "padding:0" }, ...(rows.length ? rows.map((r) => h("div", {
    class: "row", tabindex: "0", "data-id": r.id, role: "button",
    onclick: () => ctx.store.set(focusSignal(r.id)),
  }, h("div", { class: "f" }, fmtMHz(r.f_center_hz), h("small", {}, " MHz")), h("div", {}),
     h("div", { class: "meta" }, stateBadge(r.state), h("span", {}, rowSeenText(r))))) : [h("div", { class: "empty" }, "No detections here yet.")]));

  // Actions (Listen to all, Analyze, Export clip, Delete) moved to the right-click/long-press
  // context menu (T-192); see renderSignalFocus's comment.
  return h("div", {},
    h("div", {}, h("div", { class: "eyebrow" }, stateBadge("selection"), h("span", { class: "chip" }, s.name)),
      h("div", { class: "bigf" }, `${fmtMHz(s.f_lo, 2)}–${fmtMHz(s.f_hi, 2)}`, h("small", {}, " MHz")),
      h("div", { class: "sub" }, selectionSummary(s, rows.length))),
    h("div", {}, h("div", { class: "section-h" }, "Found inside ", h("em", {}, "strongest first")), inside),
    h("div", { class: "hint" }, "Right-click or long-press the selection for actions: Listen to all, Analyze, Export clip, Delete."),
  );
}

// Per-signal output panels (T-195): the packet inspector / audio scope + RDS content is a
// separately-loaded chunk (it pulls in `decode/inspector.ts`, which must stay out of Explore's
// initial bundle — ADR-0013 §1). Loaded on first signal focus, not at Explore's own mount, so a
// session that only browses the waterfall never fetches it.
let outputPanelsLoaded = false;
function ensureOutputPanels(el: HTMLElement, ctx: AppContext) {
  if (outputPanelsLoaded) return;
  outputPanelsLoaded = true;
  import("./output-panel").then((m) => m.mountOutputPanels(el, ctx));
}

const mountFocus: MountFn = (el, ctx) => {
  // Owned once and re-appended on every render (never recreated), so its subscriptions/lazy-loaded
  // content survive `el.replaceChildren` re-rendering the rest of the panel around it.
  const outputPanelsEl = h("div", { class: "out-panels-slot" });

  // T-207: signature-match/cluster caches, local to this panel (not the global store) — an
  // ephemeral per-focus fetch, same footing as output-panel.ts's own RDS decode cache. Keyed by
  // emitter id / cluster id, so switching focus back to an already-loaded signal never re-fetches.
  const matchCache = new Map<string, Loaded<SignatureMatch | null>>();
  const clusterCache = new Map<string, Loaded<Cluster | null>>();

  function loadMatch(emitterId: string) {
    if (matchCache.has(emitterId)) return;
    matchCache.set(emitterId, "loading");
    fetchSignatureMatch(ctx.client, emitterId)
      .then((r) => { matchCache.set(emitterId, r.match); render(); })
      .catch((e) => { matchCache.set(emitterId, null); ctx.store.set(toast(apiErrorText(e))); render(); });
  }
  function loadCluster(clusterId: string) {
    if (clusterCache.has(clusterId)) return;
    clusterCache.set(clusterId, "loading");
    fetchCluster(ctx.client, clusterId)
      .then((c) => { clusterCache.set(clusterId, c); render(); })
      .catch((e) => { clusterCache.set(clusterId, null); ctx.store.set(toast(clusterLoadErrorText(e))); render(); });
  }

  function render() {
    const s = ctx.store.get();
    const focus = s.focus;
    if (focus.kind === "signal") {
      const row = s.inventory.rows[focus.id];
      if (row) {
        loadMatch(row.id);
        if (row.cluster_id) loadCluster(row.cluster_id);
      }
      el.replaceChildren(
        row ? renderSignalFocus(ctx, row, matchCache.get(row.id), row.cluster_id ? clusterCache.get(row.cluster_id) : null) : h("div", { class: "empty" }, "That signal is no longer in the inventory."),
        outputPanelsEl,
      );
      ensureOutputPanels(outputPanelsEl, ctx);
      return;
    }
    if (focus.kind === "selection") {
      const sel = s.selections.list.find((x) => x.id === focus.id);
      el.replaceChildren(sel ? renderSelectionFocus(ctx, sel) : h("div", { class: "empty" }, "Select a signal or a selection."), outputPanelsEl);
      return;
    }
    el.replaceChildren(h("div", { class: "empty" }, "Select a signal or drag a region to focus it."), outputPanelsEl);
  }
  ctx.store.select(
    (s) => [s.focus, s.inventory.rows, s.selections.list, s.outputs] as const,
    render,
    { immediate: true, eq: (a, b) => a[0] === b[0] && a[1] === b[1] && a[2] === b[2] && a[3] === b[3] },
  );
};

export const mounts: AreaMounts = { inventory: mountInventory, selections: mountSelections, focus: mountFocus };
