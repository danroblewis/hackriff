// Review drawer (ADR-0013 §2, §8; T-155): tabs for Alarms, Survey report, Scheduler, Device and
// Bookmarks — the M0b–M2 panels rehomed off the old stacked page.
//
// T-445 retired the sixth, "Spectrum grid": a region-over-time waterfall over `GET /api/history`
// with **its own hand-written colormap LUT** — literally T-397's colormap divergence, a second ramp
// that stopped at cyan where the main one went on to white. The question it answered ("what did
// this region look like over this period?") is the unified surface's whole subject: pan there and
// zoom out. One renderer, one ramp. The drawer's own
// open/close and the top bar's badge stay T-150's (shell.ts toggles `#review`'s `hidden`); this
// file only fills that slot's subtree with the tab bar and the six panels.
import type { AppContext } from "../context";
import { h } from "../dom";
import { openReview, REVIEW_TABS, SETTINGS_TABS, tabGroup, type DrawerGroup, type ReviewSlice, type ReviewTab } from "../state";
import { AlarmsTab } from "./alarms";
import { BookmarksTab } from "./bookmarks";
import { DeviceTab } from "./device";
import { ReportTab } from "./report";
import { SchedulerTab } from "./scheduler";

interface Panel { el(): HTMLElement; activate(region: ReviewSlice["region"]): void; deactivate?(): void }

const LABELS: Record<ReviewTab, string> = {
  alarms: "Alarms",
  report: "Survey report",
  scheduler: "Scheduler",
  device: "Device & display",
  bookmarks: "Bookmarks",
};

const TABS: readonly { id: ReviewTab; label: string }[] =
  [...REVIEW_TABS, ...SETTINGS_TABS].map((id) => ({ id, label: LABELS[id] }));

/** T-1007: the drawer wears the name of the group its tab is in, and shows only that group's tabs.
 * Review is anomalies/alarms and the report read against them; everything configurable is Settings. */
export const GROUP_TITLE: Record<DrawerGroup, string> = { review: "Review", settings: "Settings" };

export function mountReview(el: HTMLElement, ctx: AppContext) {
  const { store, client, token } = ctx;

  const panels: Record<ReviewTab, Panel> = {
    alarms: new AlarmsTab(client, store),
    report: new ReportTab(client, store, token),
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
  const title = h("div", { class: "section-h" }, GROUP_TITLE.review);
  const head = h("div", { class: "rv-head" }, title, closeBtn);
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
    // T-1007: the drawer names its group and hides the other group's tabs, so Review holds nothing
    // configurable and Settings holds no alarm.
    const group = tabGroup(tab);
    title.textContent = GROUP_TITLE[group];
    tabBar.setAttribute("aria-label", GROUP_TITLE[group]);
    for (const [id, btn] of tabBtns) btn.hidden = tabGroup(id) !== group;
    for (const [id, btn] of tabBtns) btn.setAttribute("aria-selected", String(id === tab));
    for (const [id, wrap] of tabPanels) wrap.hidden = id !== tab;
    if (activeTab && activeTab !== tab) panels[activeTab].deactivate?.();
    activeTab = open ? tab : null;
    if (!open) return;
    panels[tab].activate(region);
  }, { immediate: true });
}
