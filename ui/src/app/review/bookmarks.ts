// Bookmarks tab (ADR-0013 §2, §4.3, §8; T-155) over `/api/bookmarks*`. Thin client: reuses
// ui/src/controls/bookmarks.ts's wire types and the name-length clip unchanged; this file renders
// the list/add/rename/delete affordances as the drawer's own DOM and mirrors the list into the
// store's `bookmarks` slice, which T-152 reads to draw markers on the live view when present.
import { BOOKMARK_NAME_MAX, type Bookmark, type NewBookmark } from "../../controls/bookmarks";
import type { ControlClient } from "../../controls/client";
import { formatFrequency } from "../../controls/freq";
import { fmtBandwidth } from "../../axis";
import { h } from "../dom";
import { requestGoto, setMode, setBookmarks, toggleReview, toast, type AppState } from "../state";
import type { Store } from "../store";
import { errText, table, td } from "./util";

/** Form fields → a `POST /api/bookmarks` body; null when the frequency can't be read. Pure, so the
 * MHz/kHz parsing and the server's own name clip length are unit-tested without a DOM. */
export function newBookmarkFromForm(fields: { name: string; freqMHz: string; bandwidthKHz: string; kind: "marker" | "bookmark" }): NewBookmark | null {
  const hz = Number(fields.freqMHz) * 1e6;
  if (!Number.isFinite(hz) || hz <= 0) return null;
  const name = Array.from(fields.name.trim().replace(/\s+/g, " ")).slice(0, BOOKMARK_NAME_MAX).join("") || formatFrequency(hz);
  const bw = Number(fields.bandwidthKHz) * 1e3;
  const out: NewBookmark = { kind: fields.kind, name, f_center_hz: hz };
  if (Number.isFinite(bw) && bw > 0) out.bandwidth_hz = bw;
  return out;
}

export class BookmarksTab {
  private list: Bookmark[] = [];
  private loaded = false;
  private readonly info = h("div", { class: "rv-info hint" });
  private readonly body = h("div", {});
  private readonly name = h("input", { class: "mono", placeholder: "name" });
  private readonly freq = h("input", { class: "mono", inputmode: "decimal", placeholder: "433.92 (MHz)" });
  private readonly bandwidth = h("input", { class: "mono", inputmode: "decimal", placeholder: "bandwidth kHz (optional)" });
  private readonly kind = h("select", {}, h("option", { value: "marker" }, "marker"), h("option", { value: "bookmark" }, "bookmark"));
  private readonly root: HTMLElement;

  constructor(private client: ControlClient, private store: Store<AppState>) {
    const form = h("form", { class: "rv-form", onsubmit: (e) => { e.preventDefault(); void this.add(); } },
      this.name, this.freq, this.bandwidth, this.kind,
      h("button", { class: "mini", type: "submit" }, "Add"));
    this.root = h("div", { class: "rv-panel" }, form, this.info, this.body);
  }

  el(): HTMLElement { return this.root; }

  activate() { if (!this.loaded) void this.load(); }

  private async load() {
    this.loaded = true;
    this.info.textContent = "loading…";
    try {
      const r = await this.client.get<{ bookmarks: Bookmark[] }>("/api/bookmarks");
      this.list = r.bookmarks.sort((a, b) => a.f_center_hz - b.f_center_hz);
      this.store.set(setBookmarks(this.list));
      this.info.textContent = this.list.length ? "" : "No markers or bookmarks yet.";
      this.render();
    } catch (e) {
      this.info.textContent = errText(e);
      this.store.set(setBookmarks([]));
    }
  }

  private async add() {
    const b = newBookmarkFromForm({
      name: (this.name as HTMLInputElement).value, freqMHz: (this.freq as HTMLInputElement).value,
      bandwidthKHz: (this.bandwidth as HTMLInputElement).value, kind: (this.kind as HTMLSelectElement).value as "marker" | "bookmark",
    });
    if (!b) { this.info.textContent = "enter a frequency in MHz"; return; }
    try {
      await this.client.post<Bookmark>("/api/bookmarks", b);
      (this.name as HTMLInputElement).value = "";
      (this.freq as HTMLInputElement).value = "";
      (this.bandwidth as HTMLInputElement).value = "";
      this.store.set(toast(`${b.kind} "${b.name}" added`));
      await this.load();
    } catch (e) {
      this.info.textContent = errText(e);
    }
  }

  private async remove(b: Bookmark) {
    try {
      await this.client.del(`/api/bookmarks/${encodeURIComponent(b.id)}`);
      await this.load();
    } catch (e) {
      this.store.set(toast(`delete: ${errText(e)}`));
    }
  }

  private async rename(b: Bookmark) {
    const name = window.prompt(`Rename "${b.name}" to:`, b.name);
    if (name === null) return;
    const clipped = Array.from(name.trim().replace(/\s+/g, " ")).slice(0, BOOKMARK_NAME_MAX).join("");
    if (!clipped) { this.store.set(toast("name cannot be empty")); return; }
    try {
      await this.client.put<Bookmark>(`/api/bookmarks/${encodeURIComponent(b.id)}`, { name: clipped });
      await this.load();
    } catch (e) {
      this.store.set(toast(`rename: ${errText(e)}`));
    }
  }

  /** Jumps Explore to the bookmark via the same one-shot `nav` request the top bar's Go to uses
   * (ADR-0013 §3.1): a store action, not a direct retune — T-151/T-152 decide pan vs retune. */
  private jump(b: Bookmark) {
    this.store.set(setMode("explore"));
    this.store.set(requestGoto(b.f_center_hz));
    this.store.set(toggleReview);
  }

  private render() {
    if (!this.list.length) { this.body.replaceChildren(); return; }
    this.body.replaceChildren(table(["name", "frequency", "bandwidth", "kind", ""], this.list.map((b) => this.row(b))));
  }

  private row(b: Bookmark): HTMLTableRowElement {
    const acts = h("div", { class: "rv-acts" },
      h("button", { class: "mini", type: "button", onclick: () => this.jump(b) }, "Jump"),
      h("button", { class: "mini", type: "button", onclick: () => void this.rename(b) }, "Rename"),
      h("button", { class: "mini", type: "button", onclick: () => void this.remove(b) }, "Delete"));
    return h("tr", {},
      td(b.name), td((b.f_center_hz / 1e6).toFixed(6), "num"),
      td(b.bandwidth_hz ? fmtBandwidth(b.bandwidth_hz) : "—", "num"), td(b.kind),
      h("td", {}, acts));
  }
}
