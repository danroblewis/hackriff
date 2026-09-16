// Review drawer (ADR-0013 §2, §8; T-155): tabs for Alarms, Survey report, History, Scheduler,
// Device and Bookmarks — the M0b–M2 panels rehomed off the old stacked page. The drawer's own
// open/close and the top bar's badge stay T-150's (shell.ts toggles `#review`'s `hidden`); this
// file only fills that slot's subtree with the tab bar and the six panels.
import type { AppContext } from "../context";
import { h } from "../dom";
import { openReview, type ReviewSlice, type ReviewTab } from "../state";
import { AlarmsTab } from "./alarms";
import { BookmarksTab } from "./bookmarks";
import { DeviceTab } from "./device";
import { HistoryTab } from "./history";
import { ReportTab } from "./report";
import { SchedulerTab } from "./scheduler";

interface Panel { el(): HTMLElement; activate(region: ReviewSlice["region"]): void; deactivate?(): void }

const TABS: readonly { id: ReviewTab; label: string }[] = [
  { id: "alarms", label: "Alarms" },
  { id: "report", label: "Survey report" },
  // T-264 took the name "History" for the top-level catalogue surface (ADR-0017 TM-8). This tab is
  // the region-over-time *spectrum* grid, a different question, so it says so.
  { id: "history", label: "Spectrum grid" },
  { id: "scheduler", label: "Scheduler" },
  { id: "device", label: "Device" },
  { id: "bookmarks", label: "Bookmarks" },
];

export function mountReview(el: HTMLElement, ctx: AppContext) {
  const { store, client, token } = ctx;

  const panels: Record<ReviewTab, Panel> = {
    alarms: new AlarmsTab(client, store),
    report: new ReportTab(client, store, token),
    history: new HistoryTab(client, store),
    scheduler: new SchedulerTab(client),
    device: new DeviceTab(client),
    bookmarks: new BookmarksTab(client, store),
  };

  const tabBtns = new Map<ReviewTab, HTMLButtonElement>();
  const tabBar = h("div", { class: "rv-tabs", role: "tablist", "aria-label": "Review" },
    ...TABS.map(({ id, label }) => {
      const btn = h("button", { class: "rv-tab", type: "button", role: "tab", "aria-selected": "false", onclick: () => store.set(openReview(id, store.get().review.region)) }, label);
      tabBtns.set(id, btn);
      return btn;
    }));
  const closeBtn = h("button", { class: "mini", type: "button", "aria-label": "Close review", onclick: () => store.set((s) => ({ review: { ...s.review, open: false } })) }, "Close");
  const head = h("div", { class: "rv-head" }, h("div", { class: "section-h" }, "Review"), closeBtn);
  const tabPanels = new Map<ReviewTab, HTMLElement>();
  const body = h("div", { class: "rv-body" }, ...TABS.map(({ id }) => {
    const wrap = h("div", { class: "rv-tabpanel", hidden: true }, panels[id].el());
    tabPanels.set(id, wrap);
    return wrap;
  }));

  el.replaceChildren(head, tabBar, body);

  let activeTab: ReviewTab | null = null;
  store.select((s) => `${s.review.open}|${s.review.tab}|${s.review.region ? JSON.stringify(s.review.region) : ""}`, () => {
    const { open, tab, region } = store.get().review;
    for (const [id, btn] of tabBtns) btn.setAttribute("aria-selected", String(id === tab));
    for (const [id, wrap] of tabPanels) wrap.hidden = id !== tab;
    if (activeTab && activeTab !== tab) panels[activeTab].deactivate?.();
    activeTab = open ? tab : null;
    if (!open) return;
    panels[tab].activate(region);
  }, { immediate: true });
}
