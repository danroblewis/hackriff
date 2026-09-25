// T-821 (MAP-21): the Research slide-in — collections as toggleable overlay layers, and every mark
// (a collection's markers and the annotations filed in it) also a sortable, filterable ROW. The Felt
// model: the table is the map's second view. Layout: `ui/mockups/map-ui-v1.html`'s `.research`.
//
// ## How the two surfaces stay in step
//
// There is one copy of the data: the `research` slice (`./research-slice.ts`). This panel loads it
// from `/api/collections`, `/api/markers` and `/api/annotations`, and writes edits back through the
// same routes, reloading the slice after each; the canvas (`centre/surface.ts`) draws the slice's
// marks per frame through the collection layers. So a rename, a delete or a layer toggle made here is
// on the canvas on its next frame, and a mark clicked on the canvas selects — and scrolls to — its
// row here, because both only read and write `research.selected`.
//
// ## Size is inversely proportional to influence (docs/23 §10.6 P4; T-899)
//
// This is a big panel, so a bare row click only SELECTS the mark (highlights the row and its box).
// Moving the view is the small per-row "Go" button; nothing on this panel reaches a device route —
// a Go that lands on spectrum no tuned window covers gets the surface's own retune OFFER, pressed
// separately. P1: a visible close. P3: a fixed position (the right edge, full height).
//
// THIN CLIENT: no signal logic. Figures are the backend's; this orders, filters and formats them.
import type { AppContext, MountFn } from "../context";
import { h } from "../dom";
import { startPoll } from "../net";
import { apiErrorText } from "../explore/format";
import { requestGoto, toast } from "../shell-slice";
import { reviewAt } from "../centre/capture-slice";
import type { AppState } from "../state";
import {
  BOOKMARKS_ID, filterRows, researchRows, selectResearch, setCollectionVisible, setResearchData, setResearchOpen,
  sortRows, type Annotation, type Collection, type Marker, type ResearchRow, type RowFilter, type RowKind, type SortKey,
} from "./research-slice";

type Get = <T>(path: string) => Promise<T>;

/** Documented page maxima (docs/api.md): collections and markers 2000, annotations 2000. */
const PAGE = 2000;
/** A guard against a pathological store, never a silent truncation: the panel says when it binds. */
const MAX_PAGES = 10;
/** The annotation list needs a window; the panel's is the whole device range over all time. */
const ALL_F = "f_lo=0&f_hi=6000000000";
const ALL_T = "t0=0&t1=4102444800";
export const RESEARCH_REFRESH_MS = 15_000;

async function pages<T>(get: Get, path: string, pick: (r: Record<string, unknown>) => T[]): Promise<{ items: T[]; truncated: boolean }> {
  const items: T[] = [];
  let cursor: string | null = null;
  for (let i = 0; i < MAX_PAGES; i++) {
    const sep = path.includes("?") ? "&" : "?";
    const r: Record<string, unknown> = await get<Record<string, unknown>>(`${path}${sep}limit=${PAGE}${cursor === null ? "" : `&cursor=${encodeURIComponent(cursor)}`}`);
    items.push(...pick(r));
    const next: unknown = r.next_cursor;
    if (next === null || next === undefined) return { items, truncated: false };
    cursor = String(next);
  }
  return { items, truncated: true };
}

/** Every collection, every marker and every annotation — GETs only. */
export async function loadResearch(get: Get): Promise<{ collections: Collection[]; markers: Marker[]; annotations: Annotation[]; truncated: boolean }> {
  const [c, m, a] = await Promise.all([
    pages(get, "/api/collections", (r) => r.collections as Collection[]),
    pages(get, "/api/markers", (r) => r.markers as Marker[]),
    pages(get, `/api/annotations?${ALL_F}&${ALL_T}`, (r) => r.annotations as Annotation[]),
  ]);
  return { collections: c.items, markers: m.items, annotations: a.items, truncated: c.truncated || m.truncated || a.truncated };
}

/** T-823 (MAP-23): the export route — one read-only GET; `collectionId` narrows it to that collection. */
export const exportPath = (collectionId: string | null): string =>
  `/api/research/export${collectionId ? `?collection=${encodeURIComponent(collectionId)}` : ""}`;

/** The saved file's name: the collection's name when one collection is exported. Presentation only. */
export function exportFilename(collectionName: string | null): string {
  const slug = (collectionName ?? "").toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "");
  return `hackriff-research${slug ? `-${slug}` : ""}.json`;
}

/** The bundle as the file's text: the server's own JSON, pretty-printed, nothing recomputed. */
export const exportText = (bundle: unknown): string => `${JSON.stringify(bundle, null, 2)}\n`;

/** Hands `text` to the browser as a file download (the offline-first "export when online" path). */
function saveFile(name: string, text: string): void {
  const url = URL.createObjectURL(new Blob([text], { type: "application/json" }));
  const a = h("a", { href: url, download: name }) as HTMLAnchorElement;
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 10_000);
}

/** What the small per-row "Go" button writes: view arithmetic only (P4), never a device route. A
 * timed mark also reviews its window; a frequency-only pin keeps the pane's time. */
export const goRow = (r: ResearchRow) => (s: AppState): Partial<AppState> => ({
  ...(r.tStartS !== null && r.tEndS !== null ? reviewAt(r.tEndS, r.tEndS - r.tStartS)() : {}),
  ...requestGoto(r.fCenterHz)(s),
  ...selectResearch(r.key)(s),
});

/** The route a row's rename / delete goes to. */
/** A collection's own path: the layer-visibility `PUT` and the `DELETE` the panel builds (T-825). */
export const collectionPath = (id: string): string => `/api/collections/${encodeURIComponent(id)}`;

export const rowPath = (r: Pick<ResearchRow, "kind" | "id">): string =>
  `${r.kind === "marker" ? "/api/markers" : "/api/annotations"}/${encodeURIComponent(r.id)}`;
export const renameBody = (r: Pick<ResearchRow, "kind">, name: string): Record<string, string> =>
  (r.kind === "marker" ? { name } : { label: name });

/** The view window the "only in this window" filter uses: the app's one (time × frequency) window. */
export function viewFilterWindow(s: AppState): RowFilter["window"] {
  const v = s.live.view;
  if (!v) return null;
  if (s.time.live) return { loHz: v.loHz, hiHz: v.hiHz };
  const span = s.time.spanS ?? 0;
  return { loHz: v.loHz, hiHz: v.hiHz, t0S: s.time.tS - span, t1S: s.time.tS };
}

export const fmtHz = (hz: number): string =>
  hz >= 1e9 ? `${(hz / 1e9).toFixed(4)} GHz` : hz >= 1e6 ? `${(hz / 1e6).toFixed(4)} MHz` : `${(hz / 1e3).toFixed(2)} kHz`;
/** Capture-clock seconds as a UTC clock time (never the browser's clock or timezone). */
export const fmtAt = (s: number): string => `${new Date(s * 1000).toISOString().slice(11, 19)}Z`;
export function placeText(r: ResearchRow): { f: string; t: string } {
  const bw = r.fHiHz - r.fLoHz;
  const f = bw > 0 ? `${fmtHz(r.fCenterHz)} · ${fmtHz(bw)}` : fmtHz(r.fCenterHz);
  if (r.tStartS === null || r.tEndS === null) return { f, t: "all time (pin)" };
  const d = r.tEndS - r.tStartS;
  return { f, t: d > 0 ? `${fmtAt(r.tStartS)} · ${d < 10 ? d.toFixed(2) : d.toFixed(0)} s` : fmtAt(r.tStartS) };
}

const COLUMNS: readonly { key: SortKey; label: string }[] = [
  { key: "kind", label: "Kind" }, { key: "name", label: "Name" }, { key: "collection", label: "Collection" },
  { key: "freq", label: "Frequency" }, { key: "time", label: "Time" },
];

export const mountResearch: MountFn = (el, ctx: AppContext) => {
  const { store, client } = ctx;
  el.classList.add("research");
  const get: Get = <T,>(p: string) => client.get<T>(p);

  // ---- local view state: presentation only ----
  const filter: RowFilter = { kind: "all", text: "", collectionId: null, window: null };
  let onlyWindow = false;
  let sort: { key: SortKey; dir: 1 | -1 } = { key: "time", dir: -1 };
  let editing: string | null = null;
  let armedDelete: string | null = null;
  let status = "";

  const close = h("button", { type: "button", class: "research-x", "aria-label": "Close Research", title: "Close" }, "✕");
  const head = h("div", { class: "research-head" },
    h("h3", {}, "Research"), h("span", { class: "research-kbd" }, "every mark is also a row"), close);
  const collList = h("ul", { class: "research-colls", "aria-label": "Collections — each is a layer" });
  const newName = h("input", { type: "text", placeholder: "New collection name", "aria-label": "New collection name", maxlength: "120" }) as HTMLInputElement;
  const exportBtn = h("button", { type: "button", class: "research-export",
    title: "Download collections, markers, annotations and measurements as one file" }, "⤓ Export") as HTMLButtonElement;
  const newBtn = h("button", { type: "submit", class: "research-new" }, "＋ Collection");
  const newForm = h("form", { class: "research-newform", autocomplete: "off" }, newName, newBtn, exportBtn);
  const tabs = h("div", { class: "research-tabs", role: "tablist", "aria-label": "Row kind" });
  const search = h("input", { type: "search", placeholder: "Filter rows", "aria-label": "Filter rows" }) as HTMLInputElement;
  const winBox = h("input", { type: "checkbox" }) as HTMLInputElement;
  const tools = h("div", { class: "research-tools" }, search, h("label", {}, winBox, " only in this window"));
  const thead = h("tr", {});
  const tbody = h("tbody", {});
  const table = h("table", { class: "research-table" }, h("thead", {}, thead), tbody);
  const statusEl = h("p", { class: "research-status", role: "status" });
  const foot = h("p", { class: "research-foot" },
    "Durable marks only — detections live in Explore and History. Click a row to highlight its mark; Go moves the view. Edits go to the backend and both views re-render from one store.");
  el.append(head, collList, newForm, tabs, tools, h("div", { class: "research-twrap" }, table), statusEl, foot);

  const refresh = async () => {
    try {
      const d = await loadResearch(get);
      status = d.truncated ? "Too many marks to load in full: some rows are missing." : "";
      store.set(setResearchData(d));
    } catch (e) {
      status = `Could not load collections: ${apiErrorText(e)}`;
      render();
    }
  };
  /** A write, then a reload so both surfaces show the server's answer, not a guess. */
  const write = async (what: string, fn: () => Promise<unknown>) => {
    try { await fn(); } catch (e) { store.set(toast(`${what} failed: ${apiErrorText(e)}`)); }
    await refresh();
  };

  // ---- the collections: each is a layer ----
  const renderCollections = (cs: readonly Collection[]) => {
    collList.replaceChildren(...cs.map((c) => {
      const vis = h("input", { type: "checkbox", "aria-label": `Show ${c.name} on the map` }) as HTMLInputElement;
      vis.checked = c.visible;
      vis.addEventListener("change", () => {
        // The layer switch: the stored default every pane reads (view state, then the PUT).
        store.set(setCollectionVisible(c.id, vis.checked));
        void write("Layer toggle", () => client.put(collectionPath(c.id), { visible: vis.checked }));
      });
      const only = h("button", {
        type: "button", class: "research-only", "aria-pressed": String(filter.collectionId === c.id),
        title: filter.collectionId === c.id ? "Show every collection's rows" : `Show only ${c.name}'s rows`,
      }, `${c.name} · ${c.member_count}`);
      only.addEventListener("click", () => { filter.collectionId = filter.collectionId === c.id ? null : c.id; render(); });
      const li = h("li", { "data-collection": c.id }, vis,
        h("i", { class: "research-sw", style: `background:${c.color ?? "var(--cream, #f3e3bf)"}`, "aria-hidden": "true" }), only);
      if (!c.reserved) {
        const del = h("button", { type: "button", class: "research-del", title: `Delete ${c.name} and its markers` },
          armedDelete === `c:${c.id}` ? "Confirm" : "✕");
        del.addEventListener("click", () => {
          if (armedDelete !== `c:${c.id}`) { armedDelete = `c:${c.id}`; render(); return; }
          armedDelete = null;
          if (filter.collectionId === c.id) filter.collectionId = null;
          void write("Delete collection", () => client.del(collectionPath(c.id)));
        });
        li.append(del);
      } else li.append(h("small", { class: "research-reserved" }, c.id === BOOKMARKS_ID ? "bookmarks" : "built in"));
      return li;
    }));
  };

  // ---- the table ----
  const renderHead = () => {
    thead.replaceChildren(...COLUMNS.map((col) => {
      const b = h("button", { type: "button", class: "research-sort" },
        col.label, sort.key === col.key ? (sort.dir === 1 ? " ▲" : " ▼") : "");
      b.addEventListener("click", () => {
        sort = sort.key === col.key ? { key: col.key, dir: sort.dir === 1 ? -1 : 1 } : { key: col.key, dir: 1 };
        render();
      });
      return h("th", { scope: "col", "aria-sort": sort.key === col.key ? (sort.dir === 1 ? "ascending" : "descending") : "none" }, b);
    }), h("th", { scope: "col" }, h("span", { class: "research-sr" }, "Actions")));
  };

  const rowEl = (r: ResearchRow, selected: string | null) => {
    const p = placeText(r);
    const nameCell = h("td", { class: "research-name" });
    if (editing === r.key) {
      const inp = h("input", { type: "text", value: r.name, "aria-label": `Rename ${r.name}`, maxlength: "120" }) as HTMLInputElement;
      const commit = () => {
        if (editing !== r.key) return;
        editing = null;
        const name = inp.value.trim();
        if (!name || name === r.name) { render(); return; }
        void write("Rename", () => client.put(rowPath(r), renameBody(r, name)));
      };
      inp.addEventListener("keydown", (e) => {
        if (e.key === "Enter") { e.preventDefault(); commit(); } else if (e.key === "Escape") { editing = null; render(); }
      });
      inp.addEventListener("blur", commit);
      nameCell.append(inp);
      queueMicrotask(() => inp.focus());
    } else {
      nameCell.append(r.name);
      if (r.note) nameCell.append(h("div", { class: "research-note" }, r.note));
    }
    const go = h("button", { type: "button", class: "research-go", "aria-label": `Go to ${r.name}`,
      title: r.tStartS !== null ? "Move the view here and review this time" : "Move the view to this frequency" }, "Go");
    go.addEventListener("click", () => store.set(goRow(r)));
    const ren = h("button", { type: "button", class: "research-ren", "aria-label": `Rename ${r.name}`, title: "Rename" }, "✎");
    ren.addEventListener("click", () => { editing = r.key; render(); });
    const del = h("button", { type: "button", class: "research-del", "aria-label": `Delete ${r.name}`, title: "Delete" },
      armedDelete === r.key ? "Confirm" : "✕");
    del.addEventListener("click", () => {
      if (armedDelete !== r.key) { armedDelete = r.key; render(); return; }
      armedDelete = null;
      void write("Delete", () => client.del(rowPath(r)));
    });
    const tr = h("tr", { class: `research-row${r.key === selected ? " selected" : ""}`, "data-key": r.key,
      tabindex: "0", "aria-selected": String(r.key === selected) },
      h("td", {}, h("span", { class: "research-kind" }, `${r.kind === "marker" ? "marker" : "note"} · ${r.shape}`)),
      nameCell,
      h("td", { class: "research-coll" }, r.collectionName),
      h("td", { class: "research-place" }, p.f),
      h("td", { class: "research-place" }, p.t, r.tier ? h("div", { class: "research-tier" }, r.tier) : null),
      h("td", { class: "research-acts" }, go, ren, del));
    // P4: the big row only SELECTS its mark (view state). It never moves the map.
    const pick = () => store.set(selectResearch(store.get().research.selected === r.key ? null : r.key));
    tr.addEventListener("click", (e) => { if (!(e.target as HTMLElement).closest("button, input")) pick(); });
    tr.addEventListener("keydown", (e) => {
      if ((e.key === "Enter" || e.key === " ") && e.target === tr) { e.preventDefault(); pick(); }
    });
    return tr;
  };

  const render = () => {
    const s = store.get();
    const r = s.research;
    renderCollections(r.collections);
    tabs.replaceChildren(...(["all", "marker", "annotation"] as const).map((k) => {
      const b = h("button", { type: "button", role: "tab", "aria-selected": String(filter.kind === k) },
        k === "all" ? "All" : k === "marker" ? "Markers" : "Annotations");
      b.addEventListener("click", () => { filter.kind = k as RowKind | "all"; render(); });
      return b;
    }));
    renderHead();
    const all = researchRows(r);
    const rows = sortRows(filterRows(all, { ...filter, window: onlyWindow ? viewFilterWindow(s) : null }), sort.key, sort.dir);
    tbody.replaceChildren(...rows.map((row) => rowEl(row, r.selected)));
    if (rows.length === 0) {
      tbody.append(h("tr", {}, h("td", { colspan: "6", class: "research-empty" },
        !r.loaded ? "Loading collections…" : all.length === 0
          ? "No marks yet. Bookmarks appear here as frequency pins; markers and annotations filed in a collection appear as rows."
          : "No row matches this filter.")));
    }
    statusEl.textContent = status || `${rows.length} of ${all.length} marks · ${r.collections.length} collections`;
  };

  // ---- wiring ----
  close.addEventListener("click", () => store.set(setResearchOpen(false)));
  el.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && editing === null) store.set(setResearchOpen(false));
  });
  newForm.addEventListener("submit", (e) => {
    e.preventDefault();
    const name = newName.value.trim();
    if (!name) { store.set(toast("Name the collection first.")); return; }
    newName.value = "";
    void write("Create collection", () => client.post("/api/collections", { name }));
  });
  exportBtn.addEventListener("click", () => {
    // One read-only GET, then a file: the bundle is the backend's, saved as received. With a
    // collection filter on, only that collection goes.
    const id = filter.collectionId;
    const name = id ? (store.get().research.collections.find((c) => c.id === id)?.name ?? null) : null;
    exportBtn.disabled = true;
    void get<unknown>(exportPath(id))
      .then((b) => { saveFile(exportFilename(name), exportText(b)); store.set(toast(`Exported ${name ?? "all research"}.`)); })
      .catch((e) => store.set(toast(`Export failed: ${apiErrorText(e)}`)))
      .finally(() => { exportBtn.disabled = false; });
  });
  search.addEventListener("input", () => { filter.text = search.value; render(); });
  winBox.addEventListener("change", () => { onlyWindow = winBox.checked; render(); });

  store.select((s) => s.research.open, (open) => {
    el.hidden = !open;
    if (open) void refresh();
  }, { immediate: true });
  // A poll's reload must not wipe a rename being typed: the row re-renders when the edit ends.
  store.select((s) => s.research, () => { if (editing === null) render(); }, { immediate: true });
  // "Only in this window" follows the app's one (time × frequency) window as it moves.
  store.select((s) => `${s.live.view?.loHz}|${s.live.view?.hiHz}|${s.time.live ? "live" : `${s.time.tS}|${s.time.spanS}`}`,
    () => { if (onlyWindow && editing === null) render(); });
  // A mark picked on the canvas: bring its row into view.
  store.select((s) => s.research.selected, (key) => {
    if (!key) return;
    const tr = [...tbody.querySelectorAll<HTMLElement>("tr[data-key]")].find((x) => x.dataset.key === key);
    tr?.scrollIntoView?.({ block: "nearest" });
  });
  // The collection layers draw whether or not the panel is open, so the data is kept fresh always.
  startPoll(refresh, RESEARCH_REFRESH_MS);
};
