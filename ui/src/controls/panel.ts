// SDR control panel (T-051) over the T-050 control API. Driven by GET /api/control/state (polled,
// and refreshed after every action); device controls are disabled with the server's reason on a
// replay (`not_live`), during a re-plumb and while a device request is in flight (no double
// submit). Display zoom, colour scale and peak hold are client-side. See ui/CONTROLS.md.
import * as ax from "../axis";
import type { Waterfall } from "../waterfall";
import { BookmarkPanel, type Bookmark } from "./bookmarks";
import { type ControlClient, reactionTo } from "./client";
import { DEFAULT_STEP, STEPS, formatFrequency, parseFrequency, shiftCenter } from "./freq";
import {
  AVERAGING_MAX, FFT_SIZES, GATED_ROWS_PER_S, ROW_RATES, type ControlState, type GainControl, type PanelModel,
  panelModel, recordingText, segmentNotice,
} from "./model";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const POLL_MS = 2000;

export interface LiveHooks {
  geometry(): ax.Geometry | null;
  waterfall(): Waterfall | null;
  zoomTo(loHz: number, hiHz: number): void;
  /** Apply this view once a retuned stream header arrives (a pan past the edge). */
  expectView(v: ax.View | null): void;
  setMarkers(list: readonly Bookmark[]): void;
  onReauth(): void;
}

const option = (value: string, text: string) => {
  const o = document.createElement("option");
  o.value = value;
  o.textContent = text;
  return o;
};

/** Sets an input's value unless the user is editing it. */
function setIdle(el: HTMLInputElement | HTMLSelectElement, value: string) {
  if (document.activeElement !== el && el.value !== value) el.value = value;
}

export class ControlPanel {
  readonly bookmarks: BookmarkPanel;
  private state: ControlState | null = null;
  private model: PanelModel | null = null;
  private pending = false;
  private prevRun: { segment: number; content_class: string } | null = null;
  private gainKey = "";
  private rateKey = "";
  private offer: { centerHz: number; view: ax.View } | null = null;

  constructor(private client: ControlClient, private live: LiveHooks) {
    this.bookmarks = new BookmarkPanel(client, {
      geometry: () => live.geometry(),
      zoomTo: (lo, hi) => live.zoomTo(lo, hi),
      canRetune: () => !!this.model?.device.enabled,
      retune: (hz) => void this.tune(hz),
      setMarkers: (l) => live.setMarkers(l),
      message: (t, bad) => this.message(t, bad),
      fail: (e) => this.fail(e),
    });
    this.buildStatic();
    this.wire();
  }

  start() {
    void this.refresh();
    void this.bookmarks.load();
    window.setInterval(() => { if (document.visibilityState === "visible") void this.refresh(); }, POLL_MS);
    // Colour scale and peak hold are client-side; re-applied so a rebuilt waterfall keeps them.
    window.setInterval(() => this.applyScale(), 500);
  }

  private get center(): number | undefined { return this.state?.tuning?.center_hz ?? this.state?.run?.center_hz; }
  private get span(): number | undefined { return this.state?.tuning?.sample_rate_hz ?? this.state?.run?.sample_rate_hz; }

  // ---- wiring ---------------------------------------------------------------------------------

  private buildStatic() {
    $("ctl-step").replaceChildren(...STEPS.map((s, i) => option(String(i), s.label)));
    $<HTMLSelectElement>("ctl-step").value = String(DEFAULT_STEP);
    $("ctl-fft").replaceChildren(...FFT_SIZES.map((n) => option(String(n), String(n))));
    $("ctl-speed").replaceChildren(...ROW_RATES.map((r) => option(String(r), `${r} rows/s`)));
    const avg = $<HTMLInputElement>("ctl-avg");
    avg.min = "1";
    avg.max = String(AVERAGING_MAX);
  }

  private wire() {
    $<HTMLFormElement>("ctl-center-form").addEventListener("submit", (e) => {
      e.preventDefault();
      const input = $<HTMLInputElement>("ctl-center"), hz = parseFrequency(input.value, this.center);
      if (hz === null) { this.message(`cannot read "${input.value}" as a frequency (e.g. 101.3M, 433.92 MHz, +25k)`, true); return; }
      input.blur();
      void this.tune(hz);
    });
    for (const [id, dir] of [["ctl-left", -1], ["ctl-right", 1]] as const) {
      $(id).addEventListener("click", () => {
        const c = this.center, sp = this.span, s = STEPS[Number($<HTMLSelectElement>("ctl-step").value)];
        if (c === undefined || sp === undefined || !s) return;
        void this.tune(shiftCenter(c, s.step, sp, dir, this.model?.frequencyRanges));
      });
    }
    $<HTMLSelectElement>("ctl-rate").addEventListener("change", (e) => {
      const hz = Number((e.target as HTMLSelectElement).value);
      void this.deviceCall(`span ${formatFrequency(hz)}`, () => this.client.post("/api/control/rate", { sample_rate_hz: hz }));
    });
    $<HTMLInputElement>("ctl-bias").addEventListener("change", (e) => {
      const box = e.target as HTMLInputElement, enabled = box.checked;
      if (enabled && !window.confirm("Enable the bias tee?\n\nIt puts DC (about 3.3 V on a HackRF) on the antenna port. Only use it with an active antenna or LNA that expects power; it can damage passive antennas, filters, DC-shorted loops or test equipment.")) {
        box.checked = false;
        return;
      }
      void this.deviceCall(`bias tee ${enabled ? "on" : "off"}`, () => this.client.post("/api/control/bias_tee", { enabled }));
    });
    const display = (field: string, value: number) =>
      void this.call(`${field} ${value}`, () => this.client.post("/api/control/display", { [field]: value }));
    $("ctl-fft").addEventListener("change", (e) => display("fft_size", Number((e.target as HTMLSelectElement).value)));
    $("ctl-speed").addEventListener("change", (e) => display("rows_per_s", Number((e.target as HTMLSelectElement).value)));
    $("ctl-avg").addEventListener("change", (e) => {
      const n = Math.round(Number((e.target as HTMLInputElement).value));
      if (n >= 1 && n <= AVERAGING_MAX) display("averaging", n);
      else this.message(`averaging must be 1..${AVERAGING_MAX} rows (1 = off)`, true);
    });
    $("ctl-pause").addEventListener("click", () => {
      const paused = !!this.state?.run?.display.paused;
      void this.call(paused ? "resume spectrum" : "pause spectrum", () => this.client.post(`/api/control/${paused ? "resume" : "pause"}`));
    });
    for (const id of ["ctl-auto", "ctl-lo", "ctl-hi", "ctl-peak"]) $(id).addEventListener("change", () => this.applyScale(true));
    $("ctl-peak-reset").addEventListener("click", () => this.live.waterfall()?.resetPeak());
    $("ctl-rec-start").addEventListener("click", () => {
      const label = $<HTMLInputElement>("ctl-rec-label").value.trim(), maxS = Number($<HTMLInputElement>("ctl-rec-max").value);
      const body: { label?: string; max_s?: number } = {};
      if (label) body.label = label;
      if (maxS > 0) body.max_s = maxS;
      void this.call("record start", () => this.client.post("/api/control/record/start", body));
    });
    $("ctl-rec-stop").addEventListener("click", () => void this.call("record stop", () => this.client.post("/api/control/record/stop")));
    $("ctl-offer-yes").addEventListener("click", () => {
      const o = this.offer;
      this.hideOffer();
      if (o) { this.live.expectView(o.view); void this.tune(o.centerHz); }
    });
    $("ctl-offer-no").addEventListener("click", () => this.hideOffer());
  }

  // ---- actions --------------------------------------------------------------------------------

  /** Retunes the centre (explicit action). A re-plumb into another class can take up to 30 s. */
  async tune(centerHz: number) {
    await this.deviceCall(`tune ${formatFrequency(centerHz)}`, () => this.client.post("/api/control/center", { center_hz: centerHz }));
  }

  /** A pan let go past the band edge: offer the retune instead of doing it. */
  offerRetune(centerHz: number, view: ax.View) {
    if (!this.model?.device.enabled) {
      this.message(`at the edge of the band${this.model?.device.reason ? ` (${this.model.device.reason})` : ""}`);
      return;
    }
    this.offer = { centerHz, view };
    $("ctl-offer-text").textContent = `Past the band edge: retune the centre to ${formatFrequency(centerHz)}?`;
    $("ctl-offer").hidden = false;
  }

  private hideOffer() {
    this.offer = null;
    $("ctl-offer").hidden = true;
  }

  /** A device request: one at a time, spinner while it runs, device controls disabled meanwhile. */
  private async deviceCall(what: string, fn: () => Promise<unknown>) {
    if (this.pending || (this.model && !this.model.device.enabled)) return;
    this.pending = true;
    this.render();
    this.message(`${what}…`);
    try {
      await fn();
      this.message(`${what}: done`);
    } catch (e) {
      this.live.expectView(null);
      this.fail(e);
    } finally {
      this.pending = false;
      await this.refresh();
    }
  }

  private async call(what: string, fn: () => Promise<unknown>) {
    try {
      await fn();
      this.message(`${what}: done`);
    } catch (e) {
      this.fail(e);
    }
    await this.refresh();
  }

  private fail(e: unknown) {
    const r = reactionTo(e);
    this.message(r.message, r.reaction !== "pending");
    if (r.reaction === "reauth") this.live.onReauth();
  }

  private message(text: string, bad = false) {
    const el = $("ctl-msg");
    el.textContent = text;
    el.classList.toggle("bad", bad);
  }

  async refresh() {
    try {
      const s = await this.client.get<ControlState>("/api/control/state");
      this.state = s;
      const notice = segmentNotice(this.prevRun, s.run);
      if (notice) $("ctl-notice").textContent = notice;
      if (s.run) this.prevRun = { segment: s.run.segment, content_class: s.run.content_class };
      this.render();
    } catch (e) {
      const r = reactionTo(e);
      $("ctl-run").textContent = `control state: ${r.message}`;
      if (r.reaction === "reauth") this.live.onReauth();
    }
  }

  // ---- rendering ------------------------------------------------------------------------------

  private render() {
    const s = this.state;
    if (!s) return;
    const m = (this.model = panelModel(s, this.pending));
    const run = s.run;
    const dev = $<HTMLFieldSetElement>("ctl-device");
    dev.disabled = !m.device.enabled;
    dev.title = m.device.reason;
    $("ctl-device-reason").textContent = m.device.reason;
    $("ctl-spinner").hidden = !m.busy;
    const disp = $<HTMLFieldSetElement>("ctl-server-display");
    disp.disabled = !m.display.enabled;
    disp.title = m.display.reason;

    $("ctl-run").textContent = run
      ? `${m.classText} · segment ${run.segment}${run.replumbing ? " · re-plumbing…" : ""}${run.finished ? " · finished" : ""}` +
        ` · ${s.live ? `live ${s.device?.driver ?? ""}` : "replay"} · receive only`
      : "no running pipeline";
    $("ctl-run").classList.toggle("gated", m.gated);

    const c = this.center, sp = this.span;
    const input = $<HTMLInputElement>("ctl-center");
    if (c !== undefined) { setIdle(input, formatFrequency(c)); input.placeholder = formatFrequency(c); }

    const rateKey = m.rates.join(",");
    const rate = $<HTMLSelectElement>("ctl-rate");
    if (rateKey !== this.rateKey) {
      this.rateKey = rateKey;
      rate.replaceChildren(...m.rates.map((r) => option(String(r), ax.fmtBandwidth(r))));
    }
    if (sp !== undefined) setIdle(rate, String(sp));

    this.renderGains(m.gains);
    $("ctl-bias-wrap").hidden = !m.biasTee.available;
    $<HTMLInputElement>("ctl-bias").checked = m.biasTee.on;

    if (run) {
      const d = run.display;
      setIdle($<HTMLSelectElement>("ctl-fft"), String(d.fft_size));
      const speed = $<HTMLSelectElement>("ctl-speed");
      if (![...speed.options].some((o) => o.value === String(d.rows_per_s))) speed.append(option(String(d.rows_per_s), `${d.rows_per_s} rows/s`));
      setIdle(speed, String(d.rows_per_s));
      setIdle($<HTMLInputElement>("ctl-avg"), String(d.averaging));
      $("ctl-speed-hint").textContent = m.gated && d.rows_per_s > GATED_ROWS_PER_S ? `gated class: at most ${GATED_ROWS_PER_S} rows/s are sent` : "";
      $("ctl-pause").textContent = d.paused ? "Resume spectrum" : "Pause spectrum";
      $("ctl-pause").classList.toggle("on", d.paused);
      $("paused").hidden = !d.paused;
    }

    const rec = run?.recording;
    const start = $<HTMLButtonElement>("ctl-rec-start"), stop = $<HTMLButtonElement>("ctl-rec-stop");
    start.disabled = !m.record.enabled;
    start.title = m.record.reason;
    stop.disabled = !m.record.active || !m.display.enabled;
    $("ctl-rec-status").textContent = m.record.enabled || m.record.active ? recordingText(rec, rec?.sample_rate_hz ?? sp) : m.record.reason;
    $("ctl-rec-status").classList.toggle("rec-on", m.record.active);
  }

  private renderGains(gains: GainControl[]) {
    const box = $("ctl-gains");
    const key = gains.map((g) => `${g.name}:${g.min}:${g.max}:${g.step}`).join("|");
    if (key !== this.gainKey) {
      this.gainKey = key;
      box.replaceChildren(...gains.map((g) => this.gainRow(g)));
    }
    for (const g of gains) {
      const input = box.querySelector<HTMLInputElement>(`input[data-stage="${CSS.escape(g.name)}"]`);
      if (!input) continue;
      if (g.toggle) { if (document.activeElement !== input) input.checked = g.value >= g.max; }
      else setIdle(input, String(g.value));
      const out = box.querySelector(`output[data-stage="${CSS.escape(g.name)}"]`);
      if (out && document.activeElement !== input) out.textContent = g.toggle ? "" : `${g.value} dB`;
    }
  }

  private gainRow(g: GainControl): HTMLElement {
    const label = document.createElement("label");
    label.className = g.toggle ? "gain toggle" : "gain";
    const input = document.createElement("input");
    input.dataset.stage = g.name;
    const out = document.createElement("output");
    out.dataset.stage = g.name;
    const send = (db: number) => void this.deviceCall(`${g.label} ${db} dB`, () => this.client.post("/api/control/gains", { gains: { [g.name]: db } }));
    if (g.toggle) {
      input.type = "checkbox";
      label.append(input, ` ${g.label} (${g.max} dB)`);
      input.addEventListener("change", () => send(input.checked ? g.max : g.min));
    } else {
      input.type = "range";
      input.min = String(g.min);
      input.max = String(g.max);
      input.step = String(g.step);
      input.setAttribute("aria-label", `${g.label} gain`);
      const head = document.createElement("span");
      head.append(`${g.label} `, out);
      label.append(head, input);
      input.addEventListener("input", () => { out.textContent = `${input.value} dB`; });
      input.addEventListener("change", () => send(Number(input.value)));
    }
    return label;
  }

  /** Colour scale (auto or manual dB) and peak hold onto the current waterfall. */
  private applyScale(fromUser = false) {
    const wf = this.live.waterfall();
    if (!wf) return;
    const auto = $<HTMLInputElement>("ctl-auto").checked;
    const lo = $<HTMLInputElement>("ctl-lo"), hi = $<HTMLInputElement>("ctl-hi");
    lo.disabled = hi.disabled = auto;
    if (auto) {
      wf.setScale(true);
      setIdle(lo, wf.lo.toFixed(0));
      setIdle(hi, wf.hi.toFixed(0));
    } else {
      const a = Number(lo.value), b = Number(hi.value);
      if (Number.isFinite(a) && Number.isFinite(b) && b > a) wf.setScale(false, a, b);
      else if (fromUser) this.message("colour scale: max must be above min", true);
    }
    wf.setPeakHold($<HTMLInputElement>("ctl-peak").checked);
  }
}
