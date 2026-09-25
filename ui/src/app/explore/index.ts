// Explore sidebar (inventory, selections) and focus panel mounts (ADR-0013 §8, T-151). Renders
// only what the API served; explanations are always shown as ranked suggestions, never as truth
// (CLAUDE.md "Product vision" §4).
import { analyzeWatched } from "./analyze-slice";
import { sameCursor, toast } from "../state";
// T-386: the sidebar filters selections against the *same* frequency view the centre pane places
// its boxes in — one definition, so a header-less session cannot list one set and draw another.
import { centreView, centreViewKey } from "../centre/view";
import { mountFocusSheet } from "../chrome/focus-sheet";
import { mountExploreDrawer } from "../chrome/explore-drawer";
import { mountSideChip } from "../chrome/side-chip";
import type { AppContext, AreaMounts, MountFn } from "../context";
import { h } from "../dom";
import { bindContextTrigger, openSelectionMenu, openSignalMenu, signalMenuItems } from "../menu";
import { detailActions, detailFreq, livenessLine, measuredBlock, type DetailRow } from "./detail";
import { startPoll } from "../net";
import {
  apiErrorText, classificationDistribution, explanationWhy, fmtBandwidth, fmtMHz, rasterText,
  refinedNote, unknownScorePct, unknownScoreText,
} from "./format";
import {
  fetchEmitterLookup, selectionSummary, signalFocus, signalFocusText, type EmitterLookup,
  type Loaded,
} from "./focus";
import {
  clusterChip, deleteEntry, emptyListText, explanationChip, explanationReasonText, loadInventoryRows,
  nextInventorySort, promoteEntry, recurrenceDots, renderedInventory, rowChips, rowSeenText,
  liveEdgeS, sortInventoryRows, viewWindow, windowKey, type Row,
} from "./inventory";
import { mountPresenceStream } from "./presence-stream";
import {
  foundInside, selectionStoreFor, selectionsEmptyText, selectionsInWindow, sortSelections,
  type Selection,
} from "./selections";
import {
  clusterLoadErrorText, clusterSummary, fetchCluster, fetchSignatureMatch, signatureMatchSummary,
  type Cluster, type SignatureMatch,
} from "./signature";
import {
  focusSelection, focusSignal, removeInventoryRowLocal, restoreInventoryRowLocal, setInventorySort,
  setInventoryTab, type InventorySortKey, type InventoryTab, type InventoryWindow,
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
    // T-587: the artefact chip is its own kind (never `rowChips`'/`clusterChip`'s family/flag/
    // signature classes), and its reason is a backend-rendered sentence — "image of the 100.8 MHz
    // carrier" — read straight off `relation.reason` and rendered as visible text, not only a
    // hover title, so it can't be missed the way T-587's field report was.
    const artifact = explanationChip(r);
    const reasonText = explanationReasonText(r);
    const chips = [...rowChips(r), ...(cluster ? [cluster] : []), ...(artifact ? [artifact] : [])]
      .map((c) => h("span", { class: `chip ${c.cls}`, title: c.title }, c.text));
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
      h("div", { class: "meta" }, ...chips, h("span", { class: "mono" }, fmtBandwidth(r.bandwidth_hz)), dotsEl, h("span", {}, rowSeenText(r)),
        reasonText ? h("span", { class: "artifact-reason" }, reasonText) : null),
    );
  }

  function render() {
    const s = ctx.store.get();
    // T-389: one filtered collection, shared with the waterfall's boxes (`renderedInventory`).
    // The list used to apply its own `filter(r => r.state === …)` while the boxes applied a
    // second, stricter one; two predicates over one store is how a "1 candidate" heading came to
    // stand beside several drawn candidate boxes.
    const { listed: { confirmed, candidate } } = renderedInventory(s.inventory.rows, s.focus.kind === "signal" ? s.focus.id : null);
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
  // T-388: while the view follows the live edge, a continuing signal's box top tracks it over the
  // `presence` stream instead of waiting up to 5 s for the poll above (which still owns creating,
  // arbitrating and windowing the rows — the push only ever extends one that is already here). A
  // paused or scrubbed view unsubscribes and is left on exactly this poll.
  mountPresenceStream(ctx);
};

// ---- selections (sidebar) ----

const mountSelections: MountFn = (el, ctx) => {
  selectionStoreFor(ctx); // creates the shared store and mirrors it into state.selections
  const head = h("div", { class: "side-sel-head" }, h("div", { class: "h" }, "Selections ", h("em", {}, "this window")));
  const list = h("div", { class: "sel-list" });
  // T-386: what the window left out, when it left something out. A selection filtered away is
  // elsewhere, not gone, and the count says so rather than the list silently shrinking.
  const elsewhere = h("div", { class: "tab-note hint" });
  el.replaceChildren(head, elsewhere, list);
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

  // T-386: the sidebar is a view of the window, like every other surface. It used to render the
  // page's whole set — every frequency, all time — beside a waterfall showing one band's twenty
  // seconds, which is the window rule broken by *widening*: the panel looked full by answering a
  // bigger question than the one on screen. The split is `selectionsInWindow`, and the centre
  // view's boxes take the same `listed` collection (T-389's one-collection rule).
  function render() {
    const s = ctx.store.get();
    const split = selectionsInWindow(sortSelections(s.selections.list), centreView(s), viewWindow(s));
    list.replaceChildren(...(split.listed.length
      ? split.listed.map(renderRow)
      : [h("div", { class: "empty" }, selectionsEmptyText(split))]));
    const away = split.outside + split.undecidable;
    elsewhere.textContent = split.listed.length && away
      ? `${away} more selection${away === 1 ? "" : "s"} outside this window`
      : "";
  }

  // The window moves for reasons the selection list never sees — the live edge advancing, a scrub,
  // a retune — so this subscribes to `windowKey` as well (T-384): a panel watching only its own
  // data goes on answering about the window it was mounted in.
  ctx.store.select(
    (s) => [s.selections.list, s.focus, `${windowKey(s)}|${centreViewKey(s)}`] as const,
    render,
    { immediate: true, eq: (a, b) => a[0] === b[0] && a[1] === b[1] && a[2] === b[2] },
  );
};

// ---- focus panel ----

function stateBadge(state: string): HTMLElement { return h("span", { class: `state ${state}` }, state); }

export function renderSignalFocus(ctx: AppContext, r: Row, match: Loaded<SignatureMatch | null>, cluster: Loaded<Cluster | null>): HTMLElement {
  // T-587: same artefact chip and visible reason as the sidebar row — the focus panel is the same
  // row, so it must say the same thing.
  const artifact = explanationChip(r);
  const reasonText = explanationReasonText(r);
  const chips = [
    ...rowChips(r).map((c) => h("span", { class: `chip ${c.cls}` }, c.text)),
    ...(artifact ? [h("span", { class: `chip ${artifact.cls}` }, artifact.text)] : []),
  ];
  const flags = r.explanations[0]?.flags ?? [];
  const { centerHz } = detailFreq(r);
  const edgeS = liveEdgeS(ctx.store.get());
  const live = livenessLine(r, edgeS);
  const measured = measuredBlock(r as DetailRow, edgeS);

  // T-804: the measurements with the time they were measured over (`measured`, T-350) — a level is
  // never shown without its time, and "nothing measured" is said, never drawn as a zero.
  const kv = h("dl", { class: "kv" },
    ...measured.lines.flatMap((l) => [h("dt", {}, l.label), h("dd", {}, l.value)]),
    h("dt", {}, "Seen"), h("dd", {}, rowSeenText(r)),
    h("dt", {}, "Channel raster"), h("dd", { class: flags.includes("off-raster") ? "flag" : undefined }, rasterText(r.explanations[0]?.evidence ?? [])),
  );
  const measuredAt = h("div", { class: "at" }, measured.at ?? "No level measured yet — no linked detection.");

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

  const explanations = r.explanations.length === 0
    ? h("p", { class: "hint" }, "No explanation suggested yet — an unknown signal, which is the interesting kind.")
    : h("ol", { class: "expl" }, ...r.explanations.map((e) => h("li", { class: e.flags.length ? "flagged" : undefined },
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
  // T-320: name the cluster here too, so the panel and the list chip are visibly the same group.
  const clusterSection = h("div", {},
    h("div", { class: "section-h" },
      r.cluster_group ? `Signature cluster ${r.cluster_group.label} ` : "Cluster ",
      h("em", {}, "the same thing seen before")),
    h("p", { class: "hint" }, clusterText));

  // GAP 3: only the identity (not the latest decoded fields, e.g. RDS PS/PTY) is on the row, so
  // this shows only what's actually served — no invented decode summary.
  const identityBox = r.identity_scheme
    ? h("div", { class: "decode" },
        h("div", { class: "section-h", style: "margin-bottom:6px" }, "Identity"),
        h("dl", { class: "kv" }, h("dt", {}, r.identity_scheme), h("dd", {}, r.identity_value ?? (r.withheld ? "withheld" : "—"))))
    : null;

  // T-804: the sheet's device actions (the mockup's Listen … Delete), built from the context
  // menu's own items so a button calls exactly what the menu item calls; the menu stays for the rest
  // (Adjust band, Reset band) and for right-click on the surface.
  // docs/23 §10.6 P4: size inversely proportional to influence. The sheet is the largest surface,
  // so it only shows; every action that reaches the device or moves the map is one SMALL labelled
  // button in this compact, keyboard-reachable cluster, and nothing else in the body carries a
  // handler (ui/test/app-sheet-principles.test.ts goes red otherwise).
  const actions = h("div", { class: "actions", role: "toolbar", "aria-label": "Signal actions" },
    ...detailActions(signalMenuItems(ctx, r)).map((a) => h("button", {
      type: "button", "data-action": a.id, title: a.hint ? `${a.label} — ${a.hint}` : a.label, "aria-label": a.label,
      class: [a.primary ? "primary" : "", a.danger ? "danger" : ""].filter(Boolean).join(" ") || undefined,
      disabled: a.disabled ? "" : undefined,
      onclick: () => a.onSelect(),
    }, a.label)));

  return h("div", { class: "detail" },
    h("div", { class: "head" },
      h("div", { class: "bigf" }, fmtMHz(centerHz), h("small", {}, " MHz")),
      stateBadge(r.state),
      h("span", { class: `liveness ${live.kind}` }, live.text)),
    h("div", { class: "eyebrow" }, ...chips),
    h("div", { class: "sub" }, refinedNote(r.refined)),
    // T-587: the reason in words, next to the chip that names it, never hover-only.
    reasonText ? h("p", { class: "artifact-reason" }, reasonText) : null,
    actions,
    h("div", { class: "cols" },
      h("div", {}, h("div", { class: "section-h" }, "Measured"), kv, measuredAt),
      h("div", {}, h("div", { class: "section-h" }, "Possible explanations ", h("em", {}, "ranked suggestions, never truth")), explanations)),
    unknownBanner,
    distSection,
    matchSection,
    clusterSection,
    h("div", { class: "hint" }, "Right-click or long-press the signal for more: Adjust band, Reset band."),
    identityBox,
  );
}

export function renderSelectionFocus(ctx: AppContext, s: Selection): HTMLElement {
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

  // T-385: whether a focused emitter that is NOT among the window's rows was deleted or is merely
  // outside the window on screen. **Keyed by the window as well as the id**: "not here" is a fact
  // about one window, so an answer is only current for the window it was asked about and every new
  // window re-asks. One slot, because only one signal is focused at a time. While a re-ask is in
  // flight the previous answer still renders — existence is window-independent even though the
  // conclusion drawn from it is not — so the sentence never flashes back to "Checking…" each poll.
  let lookup: { id: string; at: string; v: Loaded<EmitterLookup> } | null = null;

  // The key an answer is current for: the window it is about, plus the identity of the row set it
  // was asked alongside. The second half closes the delete race — `deleteEntry` drops the row
  // locally before the server has answered (T-187's optimistic delete), so a lookup made in that
  // instant can still see a live entry; its `reload` then publishes a fresh row set, which re-asks
  // and settles on "deleted". Without it a real deletion could hide behind "outside the window",
  // which is this fix's own mirror image.
  let rowsGen = 0, rowsSeen: unknown = null;
  function lookupKey(rows: unknown, w: InventoryWindow | null): string {
    if (rows !== rowsSeen) { rowsSeen = rows; rowsGen++; }
    return `${w === null ? "" : `${w.t0}:${w.t1}`}#${rowsGen}`;
  }

  function lookupFor(id: string, at: string): Loaded<EmitterLookup> {
    const held = lookup && lookup.id === id ? lookup : null;
    if (held && (held.at === at || held.v === "loading")) return held.v;
    lookup = { id, at, v: "loading" };
    fetchEmitterLookup(ctx.client, id).then((v) => {
      if (lookup?.id === id && lookup.at === at) lookup = { id, at, v };
      render();
    });
    return held?.v;
  }

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
        if (lookup?.id === row.id) lookup = null; // the row is here: nothing to ask, nothing to keep
        loadMatch(row.id);
        if (row.cluster_id) loadCluster(row.cluster_id);
      }
      // T-385: the panel says *which* absence it is. It used to say "no longer in the inventory"
      // for every row not in `inventory.rows`, which for a window-scoped list is mostly the ordinary
      // case of having scrubbed or retuned away — a claim about the user's data the UI never made a
      // measurement for.
      const at = lookupKey(s.inventory.rows, s.inventory.window);
      const view = signalFocus(row, s.inventory.window, row ? undefined : lookupFor(focus.id, at));
      el.replaceChildren(
        view.kind === "row"
          ? renderSignalFocus(ctx, view.row, matchCache.get(view.row.id), view.row.cluster_id ? clusterCache.get(view.row.cluster_id) : null)
          : h("div", { class: "empty" }, signalFocusText(view)),
        outputPanelsEl,
      );
      ensureOutputPanels(outputPanelsEl, ctx);
      el.classList.remove("is-empty");
      return;
    }
    if (focus.kind === "selection") {
      const sel = s.selections.list.find((x) => x.id === focus.id);
      el.replaceChildren(sel ? renderSelectionFocus(ctx, sel) : h("div", { class: "empty" }, "Select a signal or a selection."), outputPanelsEl);
      // T-862: a selection Analyze starts a job; its section lives in the output panels.
      ensureOutputPanels(outputPanelsEl, ctx);
      el.classList.remove("is-empty");
      return;
    }
    el.replaceChildren(h("div", { class: "empty" }, "Select a signal or drag a region to focus it."), outputPanelsEl);
    if (analyzeWatched(s.analyze.jobId)) {
      // A watched analyze job keeps the panel (progress/results) on screen with nothing focused.
      ensureOutputPanels(outputPanelsEl, ctx);
      el.classList.remove("is-empty");
      return;
    }
    // T-801 round 3: nothing is focused, so this panel is just the placeholder sentence — mark it
    // so `map-layout.css` can hide it instead of covering the canvas's right edge with an empty
    // box. `.focus:empty` never fired: `render()` always fills the slot with *something* (a real
    // focus, a "not found" explanation, or this placeholder), so the element is never literally
    // empty. This class is the one case that should collapse; a real focus or an explanatory
    // "not here" state (the two branches above, which both `return` before reaching here) keeps it
    // shown, unchanged.
    el.classList.add("is-empty");
    return;
  }
  // `inventory.window` is selected too (T-385): the window is what turns "not among the rows" into
  // a sentence, and it changes without the rows changing (a re-ask that returned the same set).
  ctx.store.select(
    (s) => [s.focus, s.inventory.rows, s.inventory.window, s.selections.list, s.outputs, s.analyze.jobId] as const,
    render,
    { immediate: true, eq: (a, b) => a[0] === b[0] && a[1] === b[1] && a[2] === b[2] && a[3] === b[3] && a[4] === b[4] && a[5] === b[5] },
  );
};

// T-803: `sheet` wraps `focus` (index.html nests the slot), so it mounts after it and never replaces
// the focus panel's subtree.
export const mounts: AreaMounts = { inventory: mountInventory, selections: mountSelections, focus: mountFocus, sheet: mountFocusSheet, drawer: mountExploreDrawer, side: mountSideChip };
