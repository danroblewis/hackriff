// Explore sidebar (inventory, selections) and focus panel mounts (ADR-0013 §8, T-151). Renders
// only what the API served; explanations are always shown as ranked suggestions, never as truth
// (CLAUDE.md "Product vision" §4).
import { toast } from "../state";
import type { AppContext, AreaMounts, MountFn } from "../context";
import { h } from "../dom";
import { bindContextTrigger, openSelectionMenu, openSignalMenu } from "../menu";
import { startPoll } from "../net";
import { apiErrorText, explanationWhy, fmtBandwidth, fmtMHz, rasterText, refinedNote } from "./format";
import { selectionSummary } from "./focus";
import {
  deleteEntry, loadInventoryRows, nextInventorySort, promoteEntry, recurrenceDots, rowChips,
  rowSeenText, sortInventoryRows, type Row,
} from "./inventory";
import { foundInside, selectionStoreFor, sortSelections, type Selection } from "./selections";
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
    const chips = rowChips(r).map((c) => h("span", { class: `chip ${c.cls}` }, c.text));
    const dotsEl = r.state === "candidate" && recurrenceDots(r).length
      ? h("span", { class: "dots" }, ...recurrenceDots(r).map((v) => h("i", { style: `height:${2 + Math.round(v * 8)}px` })))
      : null;
    return h("div", {
      class: "row", role: "listitem", tabindex: "0", "data-id": r.id,
      "aria-current": current ? "true" : undefined,
      onclick: () => ctx.store.set(focusSignal(r.id)),
      onkeydown: (e: Event) => { const ke = e as KeyboardEvent; if (ke.key === "Enter" || ke.key === " ") { ke.preventDefault(); ctx.store.set(focusSignal(r.id)); } },
    },
      h("div", { class: "f" }, fmtMHz(r.f_center_hz), h("small", {}, " MHz"), isOn(ctx, r.id) ? h("span", { class: "on-air", title: "Streaming" }) : null),
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
    list.replaceChildren(...(shown.length ? shown.map(renderRow) : [h("div", { class: "empty" }, s.inventory.error ?? "Nothing here yet.")]));
  }

  ctx.store.select(
    (s) => [s.inventory, s.outputs, s.focus] as const,
    render,
    { immediate: true, eq: (a, b) => a[0] === b[0] && a[1] === b[1] && a[2] === b[2] },
  );
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

function renderSignalFocus(ctx: AppContext, r: Row): HTMLElement {
  const chips = [stateBadge(r.state), ...rowChips(r).map((c) => h("span", { class: `chip ${c.cls}` }, c.text))];
  const flags = r.explanations[0]?.flags ?? [];
  const centerHz = r.refined?.center_hz ?? r.f_center_hz;
  const bwHz = r.refined?.bandwidth_hz ?? r.bandwidth_hz;

  const kv = h("dl", { class: "kv" },
    h("dt", {}, "Bandwidth"), h("dd", {}, fmtBandwidth(bwHz)),
    h("dt", {}, "Seen"), h("dd", {}, rowSeenText(r)),
    h("dt", {}, "Channel raster"), h("dd", { class: flags.includes("off-raster") ? "flag" : undefined }, rasterText(r.explanations[0]?.evidence ?? [])),
  );

  const explanations = h("ol", { class: "expl" }, ...r.explanations.map((e) => h("li", {},
    h("span", { class: "rank" }, String(e.rank)),
    h("div", {}, h("b", {}, e.label), h("span", { class: "conf" }, e.score.toFixed(2))),
    h("div", { class: "why" }, explanationWhy(e.evidence)),
  )));

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
    kv,
    h("div", {}, h("div", { class: "section-h" }, "Possible explanations ", h("em", {}, "ranked suggestions")), explanations),
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

const mountFocus: MountFn = (el, ctx) => {
  function render() {
    const s = ctx.store.get();
    const focus = s.focus;
    if (focus.kind === "signal") {
      const row = s.inventory.rows[focus.id];
      el.replaceChildren(row ? renderSignalFocus(ctx, row) : h("div", { class: "empty" }, "That signal is no longer in the inventory."));
      return;
    }
    if (focus.kind === "selection") {
      const sel = s.selections.list.find((x) => x.id === focus.id);
      el.replaceChildren(sel ? renderSelectionFocus(ctx, sel) : h("div", { class: "empty" }, "Select a signal or a selection."));
      return;
    }
    el.replaceChildren(h("div", { class: "empty" }, "Select a signal or drag a region to focus it."));
  }
  ctx.store.select(
    (s) => [s.focus, s.inventory.rows, s.selections.list, s.outputs] as const,
    render,
    { immediate: true, eq: (a, b) => a[0] === b[0] && a[1] === b[1] && a[2] === b[2] && a[3] === b[3] },
  );
};

export const mounts: AreaMounts = { inventory: mountInventory, selections: mountSelections, focus: mountFocus };
