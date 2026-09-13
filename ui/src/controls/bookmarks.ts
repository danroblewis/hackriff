// Markers and bookmarks (T-051) over `/api/bookmarks` (T-050): user metadata only (name, centre,
// optional bandwidth and note), stored server-side. Add from a click or a selection, list, jump,
// delete. Text via textContent.
import * as ax from "../axis";
import type { ControlClient } from "./client";
import { formatFrequency } from "./freq";

export interface Bookmark {
  id: string;
  kind: "marker" | "bookmark";
  name: string;
  f_center_hz: number;
  bandwidth_hz: number | null;
  note: string | null;
  created_s: number;
  updated_s: number;
}

export interface NewBookmark { kind: "marker" | "bookmark"; name: string; f_center_hz: number; bandwidth_hz?: number }

/** hk-model `BOOKMARK_NAME_MAX` (characters). */
export const BOOKMARK_NAME_MAX = 120;

const clip = (s: string) => Array.from(s.trim().replace(/\s+/g, " ")).slice(0, BOOKMARK_NAME_MAX).join("");

/** A marker at a clicked frequency (named after it unless a name is given). */
export function bookmarkFromClick(hz: number, name = ""): NewBookmark {
  return { kind: "marker", name: clip(name) || formatFrequency(hz), f_center_hz: hz };
}

/** A bookmark covering a selection's band. */
export function bookmarkFromSelection(s: { name: string; f_lo: number; f_hi: number }): NewBookmark {
  return { kind: "bookmark", name: clip(s.name) || "Selection", f_center_hz: (s.f_lo + s.f_hi) / 2, bandwidth_hz: s.f_hi - s.f_lo };
}

export type Jump = { kind: "zoom"; loHz: number; hiHz: number } | { kind: "retune"; centerHz: number } | { kind: "outside" };

/**
 * Where "jump" goes: inside the displayed band, zoom to the bookmark (its bandwidth ×3, at least
 * ±25 kHz); outside it, retune when the device can (an explicit action), otherwise nothing.
 */
export function jumpPlan(g: ax.Geometry | null, b: Pick<Bookmark, "f_center_hz" | "bandwidth_hz">, canRetune: boolean): Jump {
  const full = g ? ax.fullView(g) : null;
  if (full && b.f_center_hz >= full.loHz && b.f_center_hz <= full.hiHz) {
    const half = Math.max(25e3, 1.5 * (b.bandwidth_hz ?? 0));
    return { kind: "zoom", loHz: b.f_center_hz - half, hiHz: b.f_center_hz + half };
  }
  return canRetune ? { kind: "retune", centerHz: b.f_center_hz } : { kind: "outside" };
}

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

export interface BookmarkHooks {
  geometry(): ax.Geometry | null;
  zoomTo(loHz: number, hiHz: number): void;
  canRetune(): boolean;
  retune(centerHz: number): void;
  setMarkers(list: readonly Bookmark[]): void;
  message(text: string, bad?: boolean): void;
  fail(e: unknown): void;
}

export class BookmarkPanel {
  private list: Bookmark[] = [];
  private pickHz: number | null = null;

  constructor(private client: ControlClient, private hooks: BookmarkHooks) {
    $("bm-add-pick").addEventListener("click", () => {
      if (this.pickHz !== null) void this.add(bookmarkFromClick(this.pickHz, $<HTMLInputElement>("bm-name").value));
    });
  }

  /** The last clicked frequency (enables "marker at last click"). */
  setPick(hz: number) {
    this.pickHz = hz;
    const b = $<HTMLButtonElement>("bm-add-pick");
    b.disabled = false;
    b.textContent = `Marker at ${formatFrequency(hz)}`;
  }

  async load() {
    try {
      const r = await this.client.get<{ bookmarks: Bookmark[] }>("/api/bookmarks");
      this.list = r.bookmarks.sort((a, b) => a.f_center_hz - b.f_center_hz);
      this.render();
    } catch (e) {
      this.hooks.fail(e);
    }
  }

  async add(b: NewBookmark) {
    try {
      await this.client.post<Bookmark>("/api/bookmarks", b);
      $<HTMLInputElement>("bm-name").value = "";
      this.hooks.message(`${b.kind} "${b.name}" added`);
      await this.load();
    } catch (e) {
      this.hooks.fail(e);
    }
  }

  private async remove(b: Bookmark) {
    try {
      await this.client.del(`/api/bookmarks/${encodeURIComponent(b.id)}`);
      await this.load();
    } catch (e) {
      this.hooks.fail(e);
    }
  }

  private jump(b: Bookmark) {
    const plan = jumpPlan(this.hooks.geometry(), b, this.hooks.canRetune());
    if (plan.kind === "zoom") this.hooks.zoomTo(plan.loHz, plan.hiHz);
    else if (plan.kind === "retune") this.hooks.retune(plan.centerHz);
    else this.hooks.message(`${b.name} is outside the displayed band and the device cannot retune here`, true);
  }

  private render() {
    this.hooks.setMarkers(this.list);
    $("bm-table").hidden = !this.list.length;
    $("bm-info").textContent = this.list.length ? "" : "No markers or bookmarks yet: click the waterfall, then add a marker, or bookmark a selection.";
    $("bm-body").replaceChildren(...this.list.map((b) => {
      const tr = document.createElement("tr");
      const td = (text: string, cls = "") => {
        const c = document.createElement("td");
        c.textContent = text;
        if (cls) c.className = cls;
        tr.append(c);
        return c;
      };
      td(b.name).title = b.note ?? "";
      td((b.f_center_hz / 1e6).toFixed(6), "num");
      td(b.bandwidth_hz ? ax.fmtBandwidth(b.bandwidth_hz) : "—", "num opt");
      td(b.kind, "opt");
      const acts = document.createElement("div");
      acts.className = "acts";
      td("").append(acts);
      for (const [label, fn] of [["Jump", () => this.jump(b)], ["Delete", () => void this.remove(b)]] as const) {
        const btn = document.createElement("button");
        btn.type = "button";
        btn.textContent = label;
        btn.addEventListener("click", fn);
        acts.append(btn);
      }
      return tr;
    }));
  }
}
