// Per-signal output panels (T-195, docs/14-ui-rewrite.md "Added scope from docs/15 §7"): the space
// T-192 freed in the right focus panel by moving actions into the context menu. A digital signal
// with an active decode gets the packet inspector (T-154, reused as-is via
// `../decode/inspector`'s `mountInspectorPanel` — no duplicated rendering logic); an FM/AM signal
// with a Listen stream gets a waveform scope plus RDS text read from
// `GET /api/inventory/{id}/decode` (T-159). Several signals can each own a panel; a tab strip
// stacks them without page-level scrolling (ADR-0013 §1).
//
// Thin client: every displayed field is exactly what the backend already computed/decoded — this
// module only decides *which* panel to show and lays out pixels (tab selection, sample→pixel
// mapping for the scope, reading named decode fields). No parsing, demodulation or classification.
//
// Loaded lazily (`explore/index.ts` dynamic `import()`, first time a signal is focused): reusing
// the packet inspector pulls in `decode/inspector.ts` + `frame-inspector.ts`, which must stay out
// of the initial bundle (ADR-0013 §1 gzip budget) — see T-179's precedent for `decode`/`review`.
import type { AppContext, MountFn } from "../context";
import { h } from "../dom";
import { getAudioSession } from "../dock/api";
import { mountInspectorPanel } from "../decode/inspector";
import { startPoll } from "../net";
import type { OutputEntry } from "../state";
import { apiErrorText, fmtMHz } from "./format";
import type { Row } from "./inventory";

// ---- pipelines (a minimal local shape: T-195 only needs emitter_id/state/outputs, so this module
// doesn't statically import decode/pipelines.ts and pull its (much larger) recipe/palette UI into
// this chunk). ----

export interface PipelineLite {
  id: string;
  emitter_id: string | null;
  state: "running" | "ended";
  outputs: readonly { id: string; kind: "inspector" | "stage" | "messages"; stream_id: string }[];
}

export type PanelSource =
  | { emitterId: string; kind: "digital"; pipelineId: string }
  | { emitterId: string; kind: "audio"; outputId: string };

/**
 * One panel per confirmed signal with an active output: a running pipeline targeting the emitter
 * with a frame ("inspector") output wins (a decode is the more specific output); otherwise a
 * live/opening Listen stream. Pure and stable-ordered (pipelines first, then dock order) so a
 * panel's identity/position doesn't jump around as unrelated state changes elsewhere.
 */
export function collectPanelSources(outputs: readonly OutputEntry[], pipelines: readonly PipelineLite[]): PanelSource[] {
  const seen = new Set<string>();
  const sources: PanelSource[] = [];
  for (const p of pipelines) {
    if (p.state !== "running" || !p.emitter_id || seen.has(p.emitter_id)) continue;
    const insp = p.outputs.find((o) => o.kind === "inspector");
    if (!insp) continue;
    seen.add(p.emitter_id);
    sources.push({ emitterId: p.emitter_id, kind: "digital", pipelineId: p.id });
  }
  for (const o of outputs) {
    if (o.kind !== "audio" || !o.emitterId || seen.has(o.emitterId)) continue;
    if (o.state !== "live" && o.state !== "opening") continue;
    seen.add(o.emitterId);
    sources.push({ emitterId: o.emitterId, kind: "audio", outputId: o.id });
  }
  return sources;
}

/** Which signal's panel a tab strip should show: keeps the current one while it's still available,
 * else the first available, else none. Pure — the click handler and an available-set change both
 * go through this so the two never disagree. */
export function nextPanelTab(current: string | null, sources: readonly PanelSource[]): string | null {
  if (current !== null && sources.some((s) => s.emitterId === current)) return current;
  return sources[0]?.emitterId ?? null;
}

/** The panel area's own empty state (§5 "a signal with no decode and no audio shows a clear, quiet
 * empty state, not an error"): null once at least one panel exists. */
export function panelsEmptyText(sources: readonly PanelSource[]): string | null {
  return sources.length === 0 ? "No active outputs. Listen or Decode a signal to see it here." : null;
}

// ---- decode fields (docs/api.md `GET /api/inventory/{id}/decode`, T-159) ----

export interface DecodeRow {
  decoder: string; recipe_id: string | null; frame_model: string; at: number;
  fields: Readonly<Record<string, unknown>>; crc: { valid: boolean }; source_session: string | null;
}
export interface DecodeResponse { decodes: readonly DecodeRow[] }

export interface RdsViewModel {
  ps: string | null; rt: string | null; pi: string | null;
  pty: number | null; tp: boolean | null; ta: boolean | null;
  updatedAtS: number;
}

const str = (v: unknown): string | null => (typeof v === "string" ? v : null);
const num = (v: unknown): number | null => (typeof v === "number" ? v : null);
const bool = (v: unknown): boolean | null => (typeof v === "boolean" ? v : null);

/**
 * Merges the RDS-family decode rows' fields into one view model (PI/group metadata, PS and
 * RadioText each land as their own row, newest first — docs/api.md's `GET .../decode`). `null`
 * when the emitter has no RDS decode yet (the panel's own quiet empty state). Every value is
 * exactly as the decoder committed it — reading a named field is not parsing.
 */
export function rdsViewModel(decodes: readonly DecodeRow[]): RdsViewModel | null {
  const rows = decodes.filter((d) => d.frame_model.startsWith("rds"));
  if (rows.length === 0) return null;
  let ps: string | null = null, rt: string | null = null, pi: string | null = null;
  let pty: number | null = null, tp: boolean | null = null, ta: boolean | null = null;
  let updatedAtS = 0;
  for (const r of rows) {
    updatedAtS = Math.max(updatedAtS, r.at);
    ps ??= str(r.fields.ps);
    rt ??= str(r.fields.rt);
    pi ??= str(r.fields.pi);
    pty ??= num(r.fields.pty);
    tp ??= bool(r.fields.tp);
    ta ??= bool(r.fields.ta);
  }
  return { ps, rt, pi, pty, tp, ta, updatedAtS };
}

/** Trims RDS's space-padded fixed-width fields (PS is 8 chars, RT up to 64) for display; a
 * presentation-only trim, not parsing — the stored value is untouched. */
export function trimRds(s: string | null): string | null {
  if (s === null) return null;
  const t = s.trim();
  return t.length ? t : null;
}

// ---- audio scope: rolling buffer + sample->pixel mapping ----

/** Keeps the most recent `capacity` audio samples (mono, −1..1), oldest dropped first. */
export class ScopeBuffer {
  private buf: Float32Array;
  private len = 0;
  constructor(readonly capacity: number) { this.buf = new Float32Array(Math.max(1, capacity)); }

  push(samples: ArrayLike<number>): void {
    const n = samples.length;
    if (n <= 0) return;
    if (n >= this.capacity) {
      for (let i = 0; i < this.capacity; i++) this.buf[i] = samples[n - this.capacity + i];
      this.len = this.capacity;
      return;
    }
    this.buf.copyWithin(0, n, this.capacity);
    for (let i = 0; i < n; i++) this.buf[this.capacity - n + i] = samples[i];
    this.len = Math.min(this.capacity, this.len + n);
  }

  /** The buffered samples, oldest first; shorter than `capacity` until it fills once. */
  snapshot(): Float32Array { return this.buf.slice(this.capacity - this.len); }
}

/**
 * Maps a window of audio samples onto an SVG polyline's `points` string: one x per pixel column
 * (nearest sample), y linear about the vertical centre, clamped to ±1. Presentation only — no
 * filtering, resampling or analysis of the audio.
 */
export function scopePoints(samples: ArrayLike<number>, width: number, height: number): string {
  const n = samples.length;
  if (n === 0 || width <= 0 || height <= 0) return "";
  const mid = height / 2;
  const pts: string[] = new Array(Math.ceil(width));
  for (let x = 0; x < pts.length; x++) {
    const i = Math.min(n - 1, Math.floor((x * n) / width));
    const v = Math.max(-1, Math.min(1, samples[i]));
    pts[x] = `${x},${(mid - v * mid).toFixed(1)}`;
  }
  return pts.join(" ");
}

// ---- DOM mount ----

const SCOPE_CAPACITY = 4800; // ~100 ms at 48 kHz — enough to see a waveform, small enough to redraw every frame

function tabLabel(rows: Readonly<Record<string, Row>>, s: PanelSource): string {
  const row = rows[s.emitterId];
  return row ? `${fmtMHz(row.f_center_hz, 3)} MHz` : s.emitterId;
}

/** The reused packet inspector's own CSS (`decode/inspector.css`) is scoped under an `.inspector`
 * ancestor class; adding it here (alongside `.out-digital`, which narrows its wide 3-column layout
 * to fit this panel's much narrower column) reuses that styling instead of duplicating it. */
export const DIGITAL_PANEL_CLASS = "out-panel out-digital inspector";
export const AUDIO_PANEL_CLASS = "out-panel out-audio";

class DigitalPanel {
  private cleanup: (() => void) | null = null;
  constructor(el: HTMLElement, ctx: AppContext, pipelineId: string) {
    this.cleanup = mountInspectorPanel(el, ctx, pipelineId);
  }
  destroy() { this.cleanup?.(); this.cleanup = null; }
}

class AudioPanel {
  private scope: ScopeBuffer = new ScopeBuffer(SCOPE_CAPACITY);
  private svg: SVGSVGElement;
  private poly: SVGPolylineElement;
  private rdsEl: HTMLElement;
  private unsubSamples: () => void;
  private stopPoll: () => void;
  private raf = 0;
  private dirty = false;

  constructor(el: HTMLElement, private ctx: AppContext, private emitterId: string, outputId: string) {
    this.svg = document.createElementNS("http://www.w3.org/2000/svg", "svg") as SVGSVGElement;
    this.svg.setAttribute("viewBox", "0 0 300 64");
    this.svg.setAttribute("preserveAspectRatio", "none");
    this.svg.setAttribute("class", "scope-svg");
    this.poly = document.createElementNS("http://www.w3.org/2000/svg", "polyline") as SVGPolylineElement;
    this.poly.setAttribute("class", "scope-line");
    this.svg.append(this.poly);
    this.rdsEl = h("div", { class: "rds-box" }, h("p", { class: "hint" }, "no station text yet."));
    el.replaceChildren(
      h("div", { class: "out-scope" }, this.svg),
      this.rdsEl,
    );
    this.unsubSamples = getAudioSession(ctx).onSamples(outputId, (pcm) => { this.scope.push(pcm); this.scheduleDraw(); });
    this.stopPoll = startPoll(() => this.loadDecode(), 3000);
  }

  private scheduleDraw() {
    if (this.dirty) return;
    this.dirty = true;
    this.raf = requestAnimationFrame(() => {
      this.dirty = false;
      this.poly.setAttribute("points", scopePoints(this.scope.snapshot(), 300, 64));
    });
  }

  private async loadDecode() {
    try {
      const r = await this.ctx.client.get<DecodeResponse>(`/api/inventory/${encodeURIComponent(this.emitterId)}/decode`);
      this.renderRds(rdsViewModel(r.decodes));
    } catch (e) {
      this.rdsEl.replaceChildren(h("p", { class: "hint" }, apiErrorText(e)));
    }
  }

  private renderRds(rds: RdsViewModel | null) {
    if (!rds) { this.rdsEl.replaceChildren(h("p", { class: "hint" }, "no station text yet.")); return; }
    const ps = trimRds(rds.ps), rt = trimRds(rds.rt);
    this.rdsEl.replaceChildren(
      h("dl", { class: "kv" },
        h("dt", {}, "Station"), h("dd", {}, ps ?? "—"),
        h("dt", {}, "RadioText"), h("dd", {}, rt ?? "—"),
        h("dt", {}, "PI"), h("dd", { class: "mono" }, rds.pi ?? "—"),
      ),
    );
  }

  destroy() {
    cancelAnimationFrame(this.raf);
    this.unsubSamples();
    this.stopPoll();
  }
}

type MountedPanel = { destroy(): void };

class OutputPanels {
  private sources: PanelSource[] = [];
  private pipelines: PipelineLite[] = [];
  private active: string | null = null;
  private mounted: MountedPanel | null = null;
  private mountedFor: string | null = null;

  private tabsEl: HTMLElement;
  private bodyEl: HTMLElement;

  constructor(el: HTMLElement, private ctx: AppContext) {
    this.tabsEl = h("div", { class: "out-tabs", role: "tablist" });
    this.bodyEl = h("div", { class: "out-body" });
    el.replaceChildren(h("div", { class: "out-panels" }, h("div", { class: "section-h" }, "Outputs"), this.tabsEl, this.bodyEl));
    ctx.store.select((s) => s.outputs, (outputs) => this.recompute(outputs), { immediate: true });
    // A running pipeline's `emitter_id`/`outputs` isn't in any store slice today (only Decode mode
    // polls `/api/pipelines`), so this panel keeps its own light poll rather than pulling in
    // `decode/pipelines.ts`'s much larger recipe/palette module.
    startPoll(async () => {
      const r = await ctx.client.get<{ pipelines: PipelineLite[] }>("/api/pipelines");
      this.pipelines = r.pipelines;
      this.recompute(ctx.store.get().outputs);
    }, 2000);
  }

  private recompute(outputs: readonly OutputEntry[]) {
    this.sources = collectPanelSources(outputs, this.pipelines);
    this.active = nextPanelTab(this.active, this.sources);
    this.render();
  }

  private selectTab(id: string) {
    if (id === this.active) return;
    this.active = id;
    this.render();
  }

  private render() {
    const rows = this.ctx.store.get().inventory.rows;
    this.tabsEl.hidden = this.sources.length < 2;
    this.tabsEl.replaceChildren(...this.sources.map((s) => h("button", {
      class: "mini", type: "button", role: "tab", "aria-selected": String(s.emitterId === this.active),
      onclick: () => this.selectTab(s.emitterId),
    }, tabLabel(rows, s))));

    const current = this.sources.find((s) => s.emitterId === this.active) ?? null;
    const currentId = current?.emitterId ?? null;
    if (currentId !== this.mountedFor) {
      this.mounted?.destroy();
      this.mounted = null;
      this.mountedFor = currentId;
      this.bodyEl.replaceChildren();
      if (current) {
        const panelEl = h("div", { class: current.kind === "digital" ? DIGITAL_PANEL_CLASS : AUDIO_PANEL_CLASS });
        this.bodyEl.append(panelEl);
        this.mounted = current.kind === "digital"
          ? new DigitalPanel(panelEl, this.ctx, current.pipelineId)
          : new AudioPanel(panelEl, this.ctx, current.emitterId, current.outputId);
      }
    }
    const empty = panelsEmptyText(this.sources);
    if (empty && !this.bodyEl.hasChildNodes()) this.bodyEl.replaceChildren(h("div", { class: "empty" }, empty));
  }
}

export const mountOutputPanels: MountFn = (el, ctx) => { new OutputPanels(el, ctx); };
