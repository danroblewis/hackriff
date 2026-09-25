// T-1008: **Scan as a buttoned map overlay.** A small Scan button in the Go-to cluster (it commands
// the radio, so it is small and sits with the other device command, the retune offer) opens a scan
// PLAN drawn as an overlay layer on the canvas — the region and the dwell steps the survey sweep
// will take, hatched over grey/unobserved cells too — with the device that will do it and its
// step/dwell. The user drags the region's edges on the map, presses Start, watches the step being
// dwelt highlighted and covered steps fade while coverage fills in beneath, and stops it from the
// same button.
//
// ## What this controller may do (thin client, CLAUDE.md)
//
//  - **It never computes a step.** Every step drawn is `plan.windows` as the backend's iterative-
//    scan engine compiled it (`GET /api/control/scan?windows=1`, T-452/T-1008). A dragged region is
//    re-priced by the server before any step is drawn for it; until then the overlay draws the
//    region alone. The plan's arithmetic — steps, pass length, duty, clipping — is the server's
//    sentence (`commitmentText` over its budget), never a client estimate.
//  - **Pricing is a read.** Opening the plan, dragging it and changing the dwell/step only `GET`
//    the price; nothing reaches a device route until Start. Start (`POST /api/control/scan`) is the
//    one commissioning act, taken on an explicit press, and Stop/Resume are the scan routes' own —
//    the same routes the Review drawer's Device tab uses (T-452/T-516). No retune is made here.
//  - **Progress is the server's.** Which step is being dwelt and which are covered come from
//    `progress.dwell_step` / `progress.step`, polled by the shell's `/api/control/state` poll that
//    already runs (`DeviceSlice.scan`), so no second poll is added.
import type { ScanBudget, ScanPlan, ScanState, ScanStep, ScanWindow } from "../../controls/model";
import { commitmentText, scanPreviewPath } from "../../controls/model";
import type { Box } from "../../surface/lattice";
import type { PaneRect } from "../../surface/surface";
import { dragRegion, scanEdgeAt, type ScanOverlayModel } from "../../surface/scanplan";
import { h } from "../dom";
import { trackOverlay } from "../chrome/dismiss";

/** The client surface this needs: the app's `ControlClient`, or a spy. */
export interface ScanClient {
  get<T = unknown>(path: string): Promise<T>;
  post<T = unknown>(path: string, body?: unknown): Promise<T>;
}

export interface ScanHost {
  client: ScanClient;
  /** The active pane's frequency window — the plan's starting region. View state only. */
  paneWindow(): { f0Hz: number; f1Hz: number } | null;
  toast(text: string): void;
  /** Something the overlay draws changed (the canvas redraws every frame anyway; this is for
   * chrome that is not per frame). Optional. */
  changed?(): void;
}

/** The default dwell a map plan opens with, s: short enough that a plan over the window you are
 * looking at is a pass of seconds to minutes, not the survey's 15 s x thousands. The box beside it
 * changes it; the server prices whatever it says, and says when it is outside the 10–30 s the
 * survey is sized for. */
export const MAP_SCAN_DWELL_S = 1;
/** The narrowest a drag may make the region, Hz (a region needs width; the server prices it). */
export const MIN_REGION_HZ = 10e3;
/** Re-price this long after the last change (typing in the dwell box). */
const PRICE_DEBOUNCE_MS = 150;

/** A plan's identity as served: a new one (a restart, a different range) re-reads its windows. */
export function planKey(s: ScanState | null): string | null {
  const p = s?.plan;
  if (!s || !p || s.state === "idle") return null;
  return [p.f_lo_hz, p.f_hi_hz, p.dwell_s, p.step ?? "fine", p.steps, p.sample_rate_hz, s.progress?.started_s ?? ""].join("|");
}

/** The price of a plan, WITH its steps — a read (`GET`), never a device route. */
export function scanPriceRequest(r: { loHz: number; hiHz: number }, dwellS: number, step: ScanStep): string {
  const path = scanPreviewPath({ f_lo_hz: r.loHz, f_hi_hz: r.hiHz, dwell_s: dwellS, step });
  return `${path}${path.includes("?") ? "&" : "?"}windows=1`;
}
/** The running sweep's own steps (one read per plan, never per poll). */
export const SCAN_WINDOWS_REQUEST = "/api/control/scan?windows=1";
/** Start/resume (the commissioning press) and stop (the surrender, never refused). */
export const SCAN_START_PATH = "/api/control/scan";
export const SCAN_STOP_PATH = "/api/control/scan/stop";
/** The body Start sends: exactly the plan that was priced. */
export function scanStartBody(r: { loHz: number; hiHz: number }, dwellS: number, step: ScanStep) {
  return { f_lo_hz: r.loHz, f_hi_hz: r.hiHz, dwell_s: dwellS, step };
}

const mhz = (hz: number) => (hz / 1e6).toFixed(hz < 1e9 ? 3 : 4);

/** The dwell box's value as a dwell, or null when it is not one. */
export function dwellFrom(text: string): number | null {
  const v = Number(text);
  return text.trim() !== "" && Number.isFinite(v) && v > 0 ? v : null;
}

/** Why the Scan button is disabled, or null when a plan can be opened. From the served state only. */
export function scanGate(s: ScanState | null | undefined, loaded: boolean): string | null {
  if (!loaded) return "waiting for the server's control state";
  if (!s) return "not_live: this server replays a recording; there is no front end to sweep";
  if (!s.available) return s.unavailable_reason ?? "this front end cannot be swept";
  return null;
}

export class ScanController {
  readonly button: HTMLButtonElement;
  readonly panel: HTMLElement;
  /** Whether the idle plan is open (a running sweep is drawn whether or not the panel is). */
  private open = false;
  private panelShown = false;
  private draft: { loHz: number; hiHz: number } | null = null;
  private dwellS = MAP_SCAN_DWELL_S;
  private step: ScanStep = "fine";
  private priced: { key: string; plan: ScanPlan; budget: ScanBudget } | null = null;
  private priceError: string | null = null;
  private priceSeq = 0;
  private priceTimer: ReturnType<typeof setTimeout> | null = null;
  private server: ScanState | null = null;
  private loaded = false;
  private running: { key: string; windows: ScanWindow[] } | null = null;
  private runFetch: string | null = null;
  private seenRunning: string | null = null;
  private busy = false;
  private dragging: "lo" | "hi" | null = null;
  private cached: ScanOverlayModel | null = null;
  private dirty = true;
  private published = "";

  private readonly label = h("span", { class: "map-scan-label" }, "Scan");
  private readonly head = h("span", { class: "map-scan-title" }, "Scan plan");
  private readonly device = h("div", { class: "map-scan-device" });
  private readonly range = h("div", { class: "map-scan-range mono" });
  private readonly dwellIn = h("input", {
    type: "number", class: "mono", min: "0.1", step: "0.1", value: String(MAP_SCAN_DWELL_S), "aria-label": "Dwell per step, seconds",
  }) as HTMLInputElement;
  private readonly stepIn = h("select", { "aria-label": "Step width" },
    h("option", { value: "fine" }, "fine steps"), h("option", { value: "coarse" }, "coarse steps")) as HTMLSelectElement;
  private readonly inputs = h("div", { class: "map-scan-inputs" },
    h("label", {}, "dwell ", this.dwellIn, " s"), this.stepIn);
  private readonly commit = h("div", { class: "map-scan-commit" });
  private readonly notes = h("div", { class: "map-scan-notes" });
  private readonly progress = h("div", { class: "map-scan-progress", role: "status" });
  private readonly hint = h("div", { class: "map-note" });
  private readonly startBtn = h("button", { type: "button", class: "map-scan-go" }, "Start") as HTMLButtonElement;
  private readonly stopBtn = h("button", { type: "button", class: "map-scan-stop" }, "Stop") as HTMLButtonElement;
  private readonly closeBtn = h("button", {
    type: "button", class: "map-offer-x map-scan-x", "aria-label": "Close the scan plan", title: "Close (Esc). A running sweep keeps running; Stop is the button.",
  }, "✕") as HTMLButtonElement;
  private readonly overlay = trackOverlay("scan-plan", () => this.close());

  constructor(private readonly host: ScanHost) {
    this.button = h("button", {
      type: "button", class: "map-scan-btn", "aria-pressed": "false", "aria-controls": "map-scan",
      title: "Scan: draw a survey-sweep plan over this view, then Start it. Commands the radio only on Start.",
    }, h("span", { class: "map-scan-ico", "aria-hidden": "true" }), this.label) as HTMLButtonElement;
    this.panel = h("div", {
      class: "map-glass map-scan", id: "map-scan", role: "group", "aria-label": "Scan plan", hidden: true,
    },
    h("div", { class: "map-layers-head" }, this.head, this.closeBtn),
    this.device, this.range, this.inputs, this.commit, this.notes, this.progress,
    h("div", { class: "map-scan-actions" }, this.startBtn, this.stopBtn), this.hint);
    this.button.addEventListener("click", () => this.press());
    this.closeBtn.addEventListener("click", () => this.close());
    this.startBtn.addEventListener("click", () => void this.startOrResume());
    this.stopBtn.addEventListener("click", () => void this.stop());
    this.dwellIn.addEventListener("input", () => {
      const d = dwellFrom(this.dwellIn.value);
      if (d !== null) { this.dwellS = d; this.schedulePrice(); }
    });
    this.stepIn.addEventListener("change", () => {
      this.step = this.stepIn.value === "coarse" ? "coarse" : "fine";
      this.schedulePrice();
    });
    this.render();
  }

  // ---- what the overlay draws ----

  /** The model the `scan` layer draws, cached between changes (it is read every frame). */
  model(): ScanOverlayModel | null {
    if (!this.dirty) return this.cached;
    this.dirty = false;
    this.cached = this.compute();
    return this.cached;
  }

  private compute(): ScanOverlayModel | null {
    const s = this.server;
    const key = planKey(s);
    if (s && key && s.plan) {
      const w = this.running?.key === key ? this.running.windows : null;
      const p = s.progress;
      return {
        state: s.state === "yielded" ? "yielded" : "running",
        loHz: w && w.length ? w[0].lo_hz : s.plan.f_lo_hz,
        hiHz: w && w.length ? w[w.length - 1].hi_hz : s.plan.f_hi_hz,
        windows: w,
        dwellStep: p?.dwell_step ?? null,
        nextStep: p ? p.step : null,
        editable: false,
      };
    }
    if (!this.open || !this.draft) return null;
    const windows = !this.dragging && this.priced?.key === this.draftKey() ? this.priced.plan.windows ?? null : null;
    return {
      state: "plan", loHz: this.draft.loHz, hiHz: this.draft.hiHz, windows,
      dwellStep: null, nextStep: null, editable: true,
    };
  }

  /** Which edge of the editable plan is under device-px column `xPx` of a pane, if any. */
  edgeAt(box: Box, rect: PaneRect, xPx: number, dpr = 1): "lo" | "hi" | null {
    return scanEdgeAt(this.model(), box, rect, xPx, 8 * Math.max(1, dpr));
  }

  /** A drag of `edge` has reached `fHz`. Nothing is priced until the drag ends. */
  dragTo(edge: "lo" | "hi", fHz: number): void {
    if (!this.draft || !Number.isFinite(fHz)) return;
    this.dragging = edge;
    this.draft = dragRegion(this.draft.loHz, this.draft.hiHz, edge, fHz, MIN_REGION_HZ);
    this.touch();
  }

  /** The drag ended: the server prices the region it now describes. */
  endDrag(): void {
    if (!this.dragging) return;
    this.dragging = null;
    this.touch();
    void this.price();
  }

  // ---- the served state ----

  /** The shell's `/api/control/state` poll delivered `scan` (compact, no windows). */
  update(s: ScanState | null | undefined, loaded = true): void {
    this.loaded = loaded;
    const prevKey = planKey(this.server);
    this.server = s ?? null;
    const key = planKey(this.server);
    if (key && this.running?.key !== key) void this.fetchRunning(key);
    if (!key) this.running = null;
    // A sweep appearing (started here, or from the Device tab) shows its progress panel once.
    if (key && key !== this.seenRunning) { this.seenRunning = key; this.showPanel(true); }
    if (prevKey && !key && !this.open) this.showPanel(false);
    this.touch();
  }

  /** The windows of the sweep that is running now — one read per plan, never per poll. */
  private async fetchRunning(key: string): Promise<void> {
    if (this.runFetch === key) return;
    this.runFetch = key;
    try {
      const a = await this.host.client.get<{ scan: ScanState }>(SCAN_WINDOWS_REQUEST);
      const k = planKey(a.scan);
      if (k && a.scan.plan?.windows) this.running = { key: k, windows: a.scan.plan.windows };
    } catch { /* the next poll tries again */ } finally {
      if (this.runFetch === key) this.runFetch = null;
      this.touch();
    }
  }

  // ---- the button and the panel ----

  private press(): void {
    const st = this.server?.state ?? "idle";
    if (st === "running" || st === "yielded") { void this.stop(); return; }
    if (this.open) { this.close(); return; }
    this.openPlan();
  }

  /** Open an idle plan over the active pane's window, priced by the server. */
  openPlan(): void {
    if (scanGate(this.server, this.loaded)) return;
    const w = this.host.paneWindow();
    if (!w || !(w.f1Hz > w.f0Hz)) { this.host.toast("There is no viewport to plan a scan over."); return; }
    this.open = true;
    this.draft = { loHz: w.f0Hz, hiHz: w.f1Hz };
    this.priced = null;
    this.priceError = null;
    this.showPanel(true);
    this.touch();
    void this.price();
  }

  close(): void {
    const running = !!planKey(this.server);
    this.open = false;
    if (!running) { this.draft = null; this.priced = null; this.priceError = null; }
    this.showPanel(false);
    this.touch();
  }

  private showPanel(on: boolean): void {
    this.panelShown = on;
    this.overlay.open(on);
  }

  private draftKey(): string {
    const d = this.draft;
    return d ? [d.loHz, d.hiHz, this.dwellS, this.step].join("|") : "";
  }

  private schedulePrice(): void {
    if (!this.open) return;
    if (this.priceTimer !== null) clearTimeout(this.priceTimer);
    this.priceTimer = setTimeout(() => { this.priceTimer = null; void this.price(); }, PRICE_DEBOUNCE_MS);
    this.touch();
  }

  /** `GET` the price of the plan as drawn — with its windows. A read; nothing moves. */
  private async price(): Promise<void> {
    const d = this.draft;
    if (!this.open || !d) return;
    const key = this.draftKey();
    const seq = ++this.priceSeq;
    try {
      const a = await this.host.client.get<{ scan: ScanState; proposed: { plan: ScanPlan; budget: ScanBudget } | null }>(
        scanPriceRequest(d, this.dwellS, this.step));
      if (seq !== this.priceSeq) return; // a newer drag or dwell superseded this answer
      this.server = a.scan ?? this.server;
      this.priced = a.proposed ? { key, plan: a.proposed.plan, budget: a.proposed.budget } : null;
      this.priceError = a.proposed ? null : "the server did not price this plan";
    } catch (e) {
      if (seq !== this.priceSeq) return;
      this.priced = null;
      this.priceError = e instanceof Error ? e.message : String(e);
    }
    this.touch();
  }

  /** THE commissioning press: Start the plan as priced (or Resume a yielded sweep). */
  private async startOrResume(): Promise<void> {
    if (this.busy) return;
    const st = this.server?.state ?? "idle";
    const d = this.draft;
    if (st === "idle" && (!d || this.priced?.key !== this.draftKey())) return; // start only what was priced
    this.busy = true;
    this.touch();
    try {
      const body = st === "yielded" ? { resume: true } : scanStartBody(d!, this.dwellS, this.step);
      const a = await this.host.client.post<{ scan: ScanState; device?: { id?: string | null } }>(SCAN_START_PATH, body);
      this.server = a.scan;
      const key = planKey(a.scan);
      if (key && a.scan.plan?.windows) this.running = { key, windows: a.scan.plan.windows };
      if (key) this.seenRunning = key;
      this.open = false;
      const dev = a.device?.id ?? a.scan.device_id ?? null;
      this.host.toast(st === "yielded" ? "Scan resumed." : `Scanning ${a.scan.plan?.steps ?? "?"} steps${dev ? ` on ${dev}` : ""}.`);
    } catch (e) {
      this.host.toast(`Scan not started: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      this.busy = false;
      this.touch();
    }
  }

  /** Stop: surrenders the radio. Never refused by the server; the plan closes with it. */
  async stop(): Promise<void> {
    if (this.busy) return;
    this.busy = true;
    this.touch();
    try {
      const a = await this.host.client.post<{ scan: ScanState }>(SCAN_STOP_PATH);
      this.server = a.scan;
      this.running = null;
      this.open = false;
      this.draft = null;
      this.priced = null;
      this.showPanel(false);
      this.host.toast("Scan stopped.");
    } catch (e) {
      this.host.toast(`Scan stop failed: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      this.busy = false;
      this.touch();
    }
  }

  private touch(): void {
    this.dirty = true;
    this.render();
    this.host.changed?.();
  }

  /** The chrome, from the state. Set-if-changed where a value is re-written every poll. */
  private render(): void {
    const s = this.server;
    const st = s?.state ?? "idle";
    const active = st === "running" || st === "yielded";
    const gate = scanGate(s, this.loaded);
    const setText = (e: HTMLElement, t: string) => { if (e.textContent !== t) e.textContent = t; };
    const setHidden = (e: HTMLElement, hide: boolean) => { if (e.hidden !== hide) e.hidden = hide; };

    setText(this.label, active ? "Stop" : "Scan");
    this.button.classList.toggle("is-active", active);
    this.button.setAttribute("aria-pressed", String(active || this.open));
    this.button.disabled = !active && gate !== null;
    this.button.title = active
      ? "Stop the survey sweep (surrenders the radio; never refused)."
      : gate ?? "Scan: draw a survey-sweep plan over this view, then Start it. Commands the radio only on Start.";

    setHidden(this.panel, !this.panelShown);
    const dev = s?.device_id ?? null;
    setText(this.device, `Device: ${dev ?? "(the source reports no identity)"}`);
    const m = this.model();
    setText(this.range, m ? `${mhz(m.loHz)}–${mhz(m.hiHz)} MHz` : "");
    setText(this.head, active ? (st === "yielded" ? "Scan yielded" : "Scanning") : "Scan plan");
    setHidden(this.inputs, active);
    const plan = active ? s?.plan ?? null : this.priced?.plan ?? null;
    const budget = active ? s?.budget ?? null : this.priced?.budget ?? null;
    const stale = !active && (this.dragging !== null || this.priced?.key !== this.draftKey());
    setText(this.commit, budget && !stale ? commitmentText(budget, plan) : this.priceError ? `Cannot scan that: ${this.priceError}` : "Pricing…");
    this.commit.classList.toggle("is-error", !active && !!this.priceError && !this.priced);
    const notes = plan && !stale ? [...plan.warnings, ...(plan.recommended_dwell ? [] : [`a ${plan.dwell_s} s dwell is outside the 10–30 s the survey is sized for`])] : [];
    setText(this.notes, notes.join(" · "));
    const p = s?.progress ?? null;
    const prog = active && p
      ? `${p.dwell_step !== null && p.dwell_step !== undefined ? `dwelling on step ${p.dwell_step + 1}` : `next step ${p.step + 1}`} of ${p.steps}`
        + ` · pass ${p.pass + 1} · ${p.steps_done} steps done`
        + (p.center_hz !== null ? ` · tuned ${mhz(p.center_hz)} MHz` : "")
        + (s?.yielded ? ` · ${s.yielded.detail}` : "")
      : "";
    setText(this.progress, prog);
    setHidden(this.progress, !prog);
    setText(this.startBtn, st === "yielded" ? "Resume" : "Start");
    setHidden(this.startBtn, st === "running");
    this.startBtn.disabled = this.busy || (st === "idle" && (stale || !this.priced || gate !== null));
    setHidden(this.stopBtn, !active);
    this.stopBtn.disabled = this.busy;
    setText(this.hint, active
      ? "The plan stays on the map while it runs: the step being dwelt is bright, covered steps fade, and coverage fills in beneath."
      : "Drag the plan's edges on the map to change the range. Nothing reaches the radio until Start.");
    this.publish(m);
  }

  /** What the overlay draws, stated on the panel for a test (and for anyone inspecting it): the
   * served windows as `[lo, hi, centre]`, exactly what `scanPlanQuads` is handed. */
  private publish(m: ScanOverlayModel | null): void {
    const sig = m ? `${m.state}|${m.loHz}|${m.hiHz}|${m.windows?.length ?? -1}|${m.windows?.[0]?.lo_hz ?? ""}|${m.dwellStep}|${m.nextStep}` : "";
    if (sig === this.published) return;
    this.published = sig;
    this.panel.dataset.plan = m ? JSON.stringify({
      state: m.state, lo_hz: m.loHz, hi_hz: m.hiHz, editable: m.editable, device_id: this.server?.device_id ?? null,
      dwell_step: m.dwellStep, next_step: m.nextStep,
      windows: m.windows ? m.windows.map((w) => [w.lo_hz, w.hi_hz, w.center_hz]) : null,
    }) : "";
  }
}
