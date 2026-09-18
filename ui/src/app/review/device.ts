// Device tab (ADR-0013 §2, §8; T-155): device status and controls over `/api/control/*`, via
// ui/src/controls/model.ts's `panelModel` (the same gating and option logic the old SDR control
// panel uses) — reused unchanged; this file only renders it as the drawer's own DOM. Never writes
// shell's `device` slice (T-150's alone); this tab keeps its own full `ControlState` locally.
// Frequency stepping, colour scale and peak hold stay with the centre live view (T-152); this tab
// covers gains, span (sample rate), bias tee, baseband filter, display settings, plus manual
// recording start/stop.
//
// T-347: there is no Pause here any more. It posted to `/api/control/pause`, which set `paused` on
// the RUN — so one browser pressing it froze every other browser's waterfall. Holding the view is
// per-viewer state and lives on the time navigator's LIVE control (ui/src/app/centre/navigators.ts)
// as the client's own time cursor. This panel is the run's shared settings; the view's own window
// is not one of them.
import type { ControlClient } from "../../controls/client";
import { formatFrequency } from "../../controls/freq";
import {
  type ControlState, type GainControl, type PanelModel, type ScanBudget, type ScanPlan,
  type ScanQuery, classLabel, fftSizeOptions, panelModel, recordingText, rowRateOptions,
  scanPanelModel, scanPreviewPath, scanQueryFrom, segmentNotice, windowLabel,
} from "../../controls/model";
import { fmtBandwidth } from "../../axis";
import { h } from "../dom";
import { errText } from "./util";

const option = (value: string, text: string) => h("option", { value }, text);

export class DeviceTab {
  private state: ControlState | null = null;
  private model: PanelModel | null = null;
  private pending = false;
  private prevRun: { segment: number; content_class: string } | null = null;
  /** T-452: the price of the sweep the inputs currently describe, from `GET /api/control/scan?…`. */
  private proposed: { plan: ScanPlan; budget: ScanBudget } | null = null;
  private poll = 0;

  private readonly notice = h("div", { class: "hint" });
  private readonly run = h("div", { class: "rv-status" });
  private readonly spinner = h("span", { class: "hint", hidden: true }, "…");
  private readonly msg = h("div", { class: "hint" });

  private readonly rate = h("select", { onchange: (e) => this.deviceCall(`span ${(e.target as HTMLSelectElement).selectedOptions[0]?.textContent}`, () => this.client.post("/api/control/rate", { sample_rate_hz: Number((e.target as HTMLSelectElement).value) })) });
  private readonly bias = h("input", { type: "checkbox", onchange: (e) => this.onBias(e.target as HTMLInputElement) });
  private readonly biasWrap = h("label", { class: "rv-toggle", hidden: true }, this.bias, " Bias tee");
  private readonly filter = h("select", { onchange: (e) => this.deviceCall(`baseband filter ${(e.target as HTMLSelectElement).selectedOptions[0]?.textContent}`, () => this.client.post("/api/control/baseband_filter", { bandwidth_hz: Number((e.target as HTMLSelectElement).value) })) });
  private readonly filterWrap = h("label", {}, "Baseband filter ", this.filter);
  private readonly gains = h("div", { class: "rv-gains" });

  private readonly fft = h("select", { onchange: (e) => this.displayCall("fft_size", Number((e.target as HTMLSelectElement).value)) });
  private readonly speed = h("select", { onchange: (e) => this.displayCall("rows_per_s", Number((e.target as HTMLSelectElement).value)) });
  private readonly window = h("select", { onchange: (e) => this.displayCall("window", (e.target as HTMLSelectElement).value) });
  private readonly avg = h("input", { type: "number", min: "1", onchange: (e) => this.onAvg(e.target as HTMLInputElement) });

  private readonly recLabel = h("input", { class: "mono", placeholder: "label (optional)" });
  private readonly recMax = h("input", { type: "number", min: "0", placeholder: "max s" });
  private readonly recStart = h("button", { class: "mini", type: "button", onclick: () => this.onRecStart() }, "Start recording");
  private readonly recStop = h("button", { class: "mini", type: "button", onclick: () => void this.call("record stop", () => this.client.post("/api/control/record/stop")) }, "Stop");
  private readonly recStatus = h("div", { class: "hint" });

  private readonly deviceFields = h("fieldset", { class: "rv-fieldset" },
    h("legend", {}, "Device"), h("div", { class: "hint" }, ""),
    h("label", {}, "Span ", this.rate), this.biasWrap, this.filterWrap,
    h("div", { class: "rv-section-h" }, "Gains"), this.gains);
  private readonly deviceReason = h("div", { class: "hint" });

  private readonly displayFields = h("fieldset", { class: "rv-fieldset" },
    h("legend", {}, "Display"),
    h("label", {}, "FFT size ", this.fft), h("label", {}, "Speed ", this.speed),
    h("label", {}, "Window ", this.window), h("label", {}, "Averaging ", this.avg));
  private readonly displayReason = h("div", { class: "hint" });

  private readonly recFields = h("div", { class: "rv-fieldset" },
    h("legend", {}, "Recording"), h("div", {}, this.recLabel, this.recMax, this.recStart, this.recStop), this.recStatus);

  // T-452: the survey sweep. Range and dwell are inputs, not a fixed button, because the pass
  // length is linear in both — and the commitment line below them is priced by the server before
  // anything starts, so a user who is about to give the radio away for 80 minutes is told first.
  private readonly scanLo = h("input", { type: "number", class: "mono", placeholder: "from MHz", onchange: () => void this.priceScan() });
  private readonly scanHi = h("input", { type: "number", class: "mono", placeholder: "to MHz", onchange: () => void this.priceScan() });
  private readonly scanDwell = h("input", { type: "number", class: "mono", min: "0.1", value: "15", placeholder: "dwell s", onchange: () => void this.priceScan() });
  private readonly scanStart = h("button", { class: "mini", type: "button", onclick: () => void this.onScanStart() }, "Start sweep");
  private readonly scanStop = h("button", { class: "mini", type: "button", onclick: () => void this.call("sweep stop", () => this.client.post("/api/control/scan/stop")) }, "Stop sweep");
  private readonly scanCommit = h("div", { class: "hint" });
  private readonly scanStatus = h("div", { class: "hint" });
  private readonly scanNotes = h("div", { class: "hint" });
  private readonly scanReason = h("div", { class: "hint" });
  private readonly scanFields = h("fieldset", { class: "rv-fieldset" },
    h("legend", {}, "Survey sweep"),
    h("div", { class: "hint" }, "Steps the tune across a range. Leave the range empty to sweep everything this front end can tune."),
    h("label", {}, "From ", this.scanLo), h("label", {}, "To ", this.scanHi), h("label", {}, "Dwell ", this.scanDwell),
    h("div", {}, this.scanStart, this.scanStop),
    this.scanCommit, this.scanStatus, this.scanNotes);

  private readonly root = h("div", { class: "rv-panel" },
    this.notice, this.run, this.spinner,
    this.deviceFields, this.deviceReason, this.scanFields, this.scanReason,
    this.displayFields, this.displayReason, this.recFields, this.msg);

  private gainKey = ""; private rateKey = ""; private filterKey = ""; private limitsKey = "";

  constructor(private client: ControlClient) {}

  el(): HTMLElement { return this.root; }

  activate() {
    if (this.poll) return;
    void this.refresh();
    // T-452: price the default sweep straight away, so the commitment line is there before the
    // user touches anything rather than appearing only once they have edited a field.
    void this.priceScan();
    this.poll = window.setInterval(() => { if (document.visibilityState === "visible") void this.refresh(); }, 2000);
  }

  deactivate() { clearInterval(this.poll); this.poll = 0; }

  private async refresh() {
    try {
      const s = await this.client.get<ControlState>("/api/control/state");
      this.state = s;
      const notice = segmentNotice(this.prevRun, s.run);
      if (notice) this.notice.textContent = notice;
      if (s.run) this.prevRun = { segment: s.run.segment, content_class: s.run.content_class };
      this.render();
    } catch (e) {
      this.run.textContent = `control state: ${errText(e)}`;
    }
  }

  private render() {
    const s = this.state;
    if (!s) return;
    const m = (this.model = panelModel(s, this.pending));
    this.renderLimits(m);
    const run = s.run;
    (this.deviceFields as HTMLFieldSetElement).disabled = !m.device.enabled;
    this.deviceReason.textContent = m.device.reason;
    this.spinner.hidden = !m.busy;
    (this.displayFields as HTMLFieldSetElement).disabled = !m.display.enabled;
    this.displayReason.textContent = m.display.reason;

    this.run.textContent = run
      ? `${classLabel(run.content_class, run.content_permitted)} · segment ${run.segment}${run.replumbing ? " · re-plumbing…" : ""}${run.finished ? " · finished" : ""}` +
        ` · ${s.live ? `live ${s.device?.driver ?? ""}` : "replay"} · receive only`
      : "no running pipeline";

    const rateKey = m.rates.join(",");
    if (rateKey !== this.rateKey) { this.rateKey = rateKey; this.rate.replaceChildren(...m.rates.map((r) => option(String(r), fmtBandwidth(r)))); }
    const sp = s.tuning?.sample_rate_hz ?? run?.sample_rate_hz;
    if (sp !== undefined && document.activeElement !== this.rate) this.rate.value = String(sp);

    this.renderGains(m.gains);
    this.biasWrap.hidden = !m.biasTee.available;
    // T-325: an unreported bias tee shows as indeterminate, never as an unticked (= off) box —
    // the DC may be on the port and nothing has said otherwise.
    (this.bias as HTMLInputElement).checked = m.biasTee.on;
    (this.bias as HTMLInputElement).indeterminate = m.biasTee.unknown;
    this.biasWrap.title = m.biasTee.unknown
      ? "Bias-tee state unknown: the device has not reported it. Do not assume it is off."
      : m.biasTee.on
        ? "Bias tee on: DC is on the antenna port."
        : "Bias tee off.";
    this.renderFilter(m.basebandFilter);

    if (run) {
      const d = run.display;
      if (document.activeElement !== this.fft) this.fft.value = String(d.fft_size);
      if (![...this.speed.options].some((o) => o.value === String(d.rows_per_s))) this.speed.append(option(String(d.rows_per_s), `${d.rows_per_s} rows/s`));
      if (document.activeElement !== this.speed) this.speed.value = String(d.rows_per_s);
      if (document.activeElement !== this.window) this.window.value = d.window;
      if (document.activeElement !== this.avg) (this.avg as HTMLInputElement).value = String(d.averaging);
    }

    this.renderScan(s);

    const rec = run?.recording;
    (this.recStart as HTMLButtonElement).disabled = !m.record.enabled;
    (this.recStop as HTMLButtonElement).disabled = !m.record.active || !m.display.enabled;
    this.recStatus.textContent = m.record.enabled || m.record.active ? recordingText(rec, rec?.sample_rate_hz ?? sp) : m.record.reason;
  }

  /**
   * T-452: the sweep control. Three things must always be visible, because each of them is a
   * promise the backend makes and the panel would otherwise swallow:
   *
   * - **the commitment**, before the button — what a pass of this range at this dwell costs;
   * - **where the sweep is**, while it runs;
   * - **why it stopped**, when the user's own tune took the radio from it. A yield is never
   *   silent, and the control that resumes it sits right there.
   */
  private renderScan(s: ControlState) {
    const m = scanPanelModel(s, this.proposed, this.pending);
    (this.scanFields as HTMLFieldSetElement).disabled = !m.gate.enabled;
    this.scanReason.textContent = m.gate.reason;
    this.scanStart.textContent = m.primary.label;
    (this.scanStart as HTMLButtonElement).disabled = !m.primary.enabled;
    (this.scanStop as HTMLButtonElement).disabled = !m.stopEnabled;
    this.scanCommit.textContent = m.commitment;
    this.scanCommit.title = m.statement;
    this.scanStatus.textContent = m.yieldText || m.progressText;
    this.scanNotes.textContent = m.notes.join(" · ");
  }

  /** What the range/dwell boxes ask for. The rule that a half-stated range sends neither end lives
   * in `scanQueryFrom`, where a test can assert the request this panel builds (T-367). */
  private scanQuery(): ScanQuery {
    return scanQueryFrom({
      loMHz: (this.scanLo as HTMLInputElement).value,
      hiMHz: (this.scanHi as HTMLInputElement).value,
      dwellS: (this.scanDwell as HTMLInputElement).value,
    });
  }

  /** Prices what the inputs currently say, without starting anything. */
  private async priceScan() {
    const path = scanPreviewPath(this.scanQuery());
    try {
      const body = await this.client.get<{ proposed: { plan: ScanPlan; budget: ScanBudget } | null }>(path);
      this.proposed = body.proposed;
      this.scanCommit.classList.remove("bad");
    } catch (e) {
      // A range this front end cannot reach is said, not swallowed: the commitment line is the
      // only place the user finds out before pressing Start.
      this.proposed = null;
      this.scanCommit.textContent = errText(e);
      this.scanCommit.classList.add("bad");
      return;
    }
    if (this.state) this.render();
  }

  private async onScanStart() {
    const resume = this.state?.scan?.state === "yielded";
    const body = resume ? { resume: true } : this.scanQuery();
    await this.call(resume ? "sweep resume" : "sweep start", () => this.client.post("/api/control/scan", body));
  }

  private renderLimits(m: PanelModel) {
    const l = m.limits;
    const key = `${l.fft_size_min}:${l.fft_size_max}:${l.averaging_max}:${l.rows_per_s_min}:${l.rows_per_s_max}:${l.windows.join(",")}`;
    if (key === this.limitsKey) return;
    this.limitsKey = key;
    this.fft.replaceChildren(...fftSizeOptions(l).map((n) => option(String(n), String(n))));
    this.speed.replaceChildren(...rowRateOptions(l).map((r) => option(String(r), `${r} rows/s`)));
    this.window.replaceChildren(...l.windows.map((w) => option(w, windowLabel(w))));
    (this.avg as HTMLInputElement).max = String(l.averaging_max);
  }

  private renderFilter(f: PanelModel["basebandFilter"]) {
    this.filterWrap.hidden = !f.available;
    if (!f.available) return;
    const key = f.options.join(",");
    if (key !== this.filterKey) { this.filterKey = key; this.filter.replaceChildren(...f.options.map((hz) => option(String(hz), fmtBandwidth(hz)))); }
    if (f.value !== null && document.activeElement !== this.filter) this.filter.value = String(f.value);
  }

  private renderGains(gains: GainControl[]) {
    const key = gains.map((g) => `${g.name}:${g.min}:${g.max}:${g.step}`).join("|");
    if (key !== this.gainKey) { this.gainKey = key; this.gains.replaceChildren(...gains.map((g) => this.gainRow(g))); }
    for (const g of gains) {
      const input = this.gains.querySelector<HTMLInputElement>(`input[data-stage="${CSS.escape(g.name)}"]`);
      if (!input || document.activeElement === input) continue;
      if (g.toggle) input.checked = g.value >= g.max; else input.value = String(g.value);
      const out = this.gains.querySelector(`output[data-stage="${CSS.escape(g.name)}"]`);
      if (out) out.textContent = g.toggle ? "" : `${g.value} dB`;
    }
  }

  private gainRow(g: GainControl): HTMLElement {
    const send = (db: number) => this.deviceCall(`${g.label} ${db} dB`, () => this.client.post("/api/control/gains", { gains: { [g.name]: db } }));
    if (g.toggle) {
      const input = h("input", { type: "checkbox", "data-stage": g.name, onchange: (e) => send((e.target as HTMLInputElement).checked ? g.max : g.min) });
      return h("label", { class: "rv-toggle" }, input, ` ${g.label} (${g.max} dB)`);
    }
    const out = h("output", { "data-stage": g.name });
    const input = h("input", {
      type: "range", "data-stage": g.name, min: String(g.min), max: String(g.max), step: String(g.step),
      "aria-label": `${g.label} gain`,
      oninput: (e) => { out.textContent = `${(e.target as HTMLInputElement).value} dB`; },
      onchange: (e) => send(Number((e.target as HTMLInputElement).value)),
    });
    return h("label", { class: "rv-gain" }, h("span", {}, `${g.label} `, out), input);
  }

  private onBias(box: HTMLInputElement) {
    if (box.checked && !window.confirm("Enable the bias tee?\n\nIt puts DC on the antenna port. Only use it with an active antenna or LNA that expects power.")) {
      box.checked = false;
      return;
    }
    this.deviceCall(`bias tee ${box.checked ? "on" : "off"}`, () => this.client.post("/api/control/bias_tee", { enabled: box.checked }));
  }

  private displayCall(field: string, value: number | string) {
    void this.call(`${field} ${value}`, () => this.client.post("/api/control/display", { [field]: value }));
  }

  private onAvg(input: HTMLInputElement) {
    const n = Math.round(Number(input.value)), max = this.model?.limits.averaging_max ?? 100;
    if (n >= 1 && n <= max) this.displayCall("averaging", n);
    else this.msg.textContent = `averaging must be 1..${max} rows (1 = off)`;
  }

  private onRecStart() {
    const label = (this.recLabel as HTMLInputElement).value.trim(), maxS = Number((this.recMax as HTMLInputElement).value);
    const body: { label?: string; max_s?: number } = {};
    if (label) body.label = label;
    if (maxS > 0) body.max_s = maxS;
    void this.call("record start", () => this.client.post("/api/control/record/start", body));
  }

  private async deviceCall(what: string, fn: () => Promise<unknown>) {
    if (this.pending || (this.model && !this.model.device.enabled)) return;
    this.pending = true;
    this.render();
    this.msg.textContent = `${what}…`;
    try {
      await fn();
      this.msg.textContent = `${what}: done`;
    } catch (e) {
      this.msg.textContent = errText(e);
    } finally {
      this.pending = false;
      await this.refresh();
    }
  }

  private async call(what: string, fn: () => Promise<unknown>) {
    try {
      await fn();
      this.msg.textContent = `${what}: done`;
    } catch (e) {
      this.msg.textContent = errText(e);
    }
    await this.refresh();
  }
}
