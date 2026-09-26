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
import { mountAnalyzeSection } from "./analyze-panel";
import { mountTracePanel } from "./analyze-trace-panel";
import type { AppContext, MountFn } from "../context";
import { h } from "../dom";
import { getAudioSession } from "../dock/api";
import { mountInspectorPanel } from "../decode/inspector";
import { startPoll } from "../net";
import type { AppState, OutputEntry } from "../state";
import { audioOutputEmitter, runningEmitters, type ServedPipeline } from "../dock/activity";
import { apiErrorText, fmtMHz } from "./format";
import { viewWindow, windowCoverage, windowEmptyText, windowKey, type Row, type ViewWindow, type WindowState } from "./inventory";
import type { WindowCoverage } from "./slice";

// ---- pipelines: the served `GET /api/pipelines` records (`servedOutputs.pipelines`, polled by
// `dock/index.ts`) — the SAME records the map's box badges read (LP-7, ADR-0015 §12.7). ----

/** A served pipeline as a test writes one out in full. */
export interface PipelineLite extends ServedPipeline {
  id: string;
  emitter_id: string | null;
  state: "running" | "ended";
  outputs: readonly { id: string; kind: "inspector" | "stage" | "messages" | "audio"; stream_id: string }[];
}

export type PanelSource =
  | { emitterId: string; kind: "rds"; pipelineId: string }
  | { emitterId: string; kind: "digital"; pipelineId: string }
  /** `outputId` is the dock entry's id (what `AudioSession` keys on). `pipelineId` is the pipeline
   * that owns the audio output (from the stream header, T-866) or null for a legacy Listen chain;
   * `rdsSibling` is a running pipeline of this emitter whose `messages` outputs are RDS, so the
   * panel shows scope + RDS as two outputs of one signal (ADR-0015 §12.7). */
  | { emitterId: string; kind: "audio"; outputId: string; pipelineId: string | null; rdsSibling: string | null };

/** `messages` output ids `rds.recipe.json` (group-info/station/radiotext) commits its rows under
 * (T-252): a running pipeline offering any of these is recognised as RDS without decoding
 * anything client-side — a structural read of the pipeline listing, the same kind of selection
 * heuristic `collectPanelSources` already made for "has an inspector output". */
const RDS_MESSAGE_OUTPUT_IDS = new Set(["group-info", "station", "radiotext"]);

/**
 * One panel per confirmed signal with an active output: a running RDS-recipe pipeline (identified
 * by its `messages` output ids) gets the dedicated accumulated RDS view ahead of the raw packet
 * inspector — the per-frame group stream is plumbing, the assembled PS/RadioText/PI/PTY readout is
 * the product (T-252, found live: the inspector was winning and hiding it). Otherwise a running
 * pipeline with a frame ("inspector") output wins over a live Listen stream (server header
 * arrived — the map badge's rule, LP-7). Pure and stable-ordered (pipelines first, then dock order) so a panel's identity/position doesn't jump
 * around as unrelated state changes elsewhere.
 */
export function collectPanelSources(outputs: readonly OutputEntry[], pipelines: readonly ServedPipeline[]): PanelSource[] {
  // One output model (ADR-0015 §12.7 / LP-7): every output, whichever API produced it, is
  // `{emitter, pipeline_id, output_id, kind}`; the widget is chosen from the kinds an emitter has.
  // Whether an audio entry is an open output, and of which emitter, is `dock/activity.ts`'s
  // `audioOutputEmitter` — the rule the map badge uses — so the two can never disagree.
  const liveAudio = new Map<string, OutputEntry>();
  const running = pipelines.filter((p) => p.state === "running");
  const emitterOf = runningEmitters(pipelines);
  for (const o of outputs) {
    const em = audioOutputEmitter(o, emitterOf);
    if (em && !liveAudio.has(em)) liveAudio.set(em, o);
  }
  const seen = new Set<string>();
  const sources: PanelSource[] = [];
  const audioFor = (emitterId: string, rds: string | null): PanelSource | null => {
    const a = liveAudio.get(emitterId);
    return a ? { emitterId, kind: "audio", outputId: a.id, pipelineId: a.pipelineId, rdsSibling: rds } : null;
  };
  for (const p of running) {
    if (!p.emitter_id || seen.has(p.emitter_id)) continue;
    const outs = p.outputs ?? [];
    const isRds = outs.some((o) => o.kind === "messages" && RDS_MESSAGE_OUTPUT_IDS.has(o.id));
    const hasInspector = outs.some((o) => o.kind === "inspector");
    if (isRds) {
      seen.add(p.emitter_id);
      // Audio + RDS are two outputs of one signal: one panel, the scope with the RDS box under it.
      sources.push(audioFor(p.emitter_id, p.id) ?? { emitterId: p.emitter_id, kind: "rds", pipelineId: p.id });
    } else if (hasInspector) {
      seen.add(p.emitter_id);
      sources.push({ emitterId: p.emitter_id, kind: "digital", pipelineId: p.id });
    }
  }
  for (const [emitterId] of liveAudio) {
    if (seen.has(emitterId)) continue;
    seen.add(emitterId);
    const a = audioFor(emitterId, null);
    if (a) sources.push(a);
  }
  return sources;
}

/** The panel sources for a store state: `outputs` (the streams this page opened) + the served
 * `GET /api/pipelines` records, exactly what `boxActivity` draws the map badges from. */
export const panelSourcesOf = (s: Pick<AppState, "outputs" | "servedOutputs">): PanelSource[] =>
  collectPanelSources(s.outputs, s.servedOutputs.pipelines);

/** The mounted panel's identity: the output it shows, not just the signal. A Listen restart (new
 * `outputId`) or RDS → audio on the same signal is a different panel and remounts; an RDS sibling
 * appearing or leaving is not (the audio panel updates in place, [[AudioPanel.update]]). */
export function panelKey(s: PanelSource | null): string | null {
  if (!s) return null;
  return `${s.emitterId}|${s.kind}|${s.kind === "audio" ? s.outputId : s.pipelineId}`;
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

// ---- the window these panels are a view over (T-384) ----

/**
 * The `/decode` request for one emitter over the **view window**, or `null` when the window is
 * unknown and nothing may be asked.
 *
 * The output/decode panels are views over the UI's one (time × frequency) window like every other
 * surface (CLAUDE.md 2026-09-16; ADR-0013 §3.3.1). They were live-edge-only: the route carried no
 * time parameter at all, so a panel scrubbed back an hour went on rendering the live edge's PS,
 * RadioText and PTY and labelled them the window's. That is the *we-have-it-but-didn't-render-it*
 * bug inverted — not an empty surface, a surface showing the **wrong window's** data, which is
 * worse because it looks right.
 *
 * `t0`/`t1` are the same [[viewWindow]] the waterfall, the Candidate list and the frequency
 * navigator name — one window, not one per surface — and they are on the **capture clock**.
 * `Date.now()` is not a fallback here any more than it is there: a replay's stamps and the
 * browser's are unrelated, and a window built from the wrong clock returns an honest zero rows
 * that reads on screen as "nothing was decoded".
 */
export function decodePath(emitterId: string, w: ViewWindow | null): string | null {
  return w === null ? null : `/api/inventory/${encodeURIComponent(emitterId)}/decode?t0=${w.t0}&t1=${w.t1}`;
}

/** What a panel's `/decode` read produced for the window it asked about. */
export type DecodeView =
  | { kind: "rows"; decodes: readonly DecodeRow[] }
  /** No window could be named, so nothing was asked. */
  | { kind: "no-window" }
  /** The window was asked about and held no decode; `coverage` says whether anything ever looked. */
  | { kind: "empty"; coverage: WindowCoverage }
  | { kind: "error"; message: string };

/**
 * What an empty decode panel says, and — as on the sidebar (T-379's `emptyListText`) — it must
 * never be one sentence. The same four-state vocabulary, read off the same `GET /api/coverage`
 * answer through the same [[windowCoverage]] helper, so the two surfaces cannot come to different
 * conclusions about one window:
 *
 * | state | what it means | sentence |
 * |---|---|---|
 * | no window known | the UI does not know which window to ask about | "Waiting for the capture window…" |
 * | window unobserved | nothing ever looked here | "Nothing was observed in this window — no data, not a quiet band." |
 * | window observed | the receiver was sampling and no decode was committed | "Nothing decoded in this window." |
 * | coverage unknown | the window was asked about; whether it was sampled is not known | "No decode listed for this window." |
 *
 * Only the third is a finding. Rendering the first or second as the third turns an absence of
 * measurement into a result — the error the waterfall's grey rule exists to stop, on this surface.
 *
 * The first two sentences come from [[windowEmptyText]] (T-387), so this panel, the Explore lists
 * and the packet inspector make the *same* claim in the *same* words; only the third and fourth —
 * the ones that are about this surface's own subject — are supplied here.
 */
export function decodeEmptyText(v: Exclude<DecodeView, { kind: "rows" }>): string {
  return windowEmptyText(v, "Nothing decoded in this window.", "No decode listed for this window.");
}

/** The `/decode` client both panels share: `AppContext["client"]` satisfies it structurally. */
export interface DecodeClient { get<T>(path: string): Promise<T> }

/**
 * Reads one emitter's decodes **for the view window**, and says which emptiness it got.
 *
 * Coverage is asked for only when the window came back with no decodes — the one case where the
 * difference between "nothing was decoded" and "nothing ever looked here" is the whole message.
 * A coverage answer that never came stays *unknown* rather than hardening into "unobserved"
 * ([[windowCoverage]] returns `null` for that), so a missing answer never becomes a measurement
 * claim.
 *
 * **This never re-decodes.** `/api/inventory/{id}/decode` serves rows the decoder already
 * committed, selected by their own capture-clock `at`; asking about a past window is a read, not a
 * re-run. Re-deriving a view by replaying the decoder over already-decoded frames would break the
 * incremental-decode invariant (CLAUDE.md: *live decoding extends the region's time extent and
 * decodes only the newly-arrived part*).
 */
export async function loadDecodeView(
  client: DecodeClient, state: WindowState, emitterId: string,
): Promise<DecodeView> {
  const w = viewWindow(state);
  const path = decodePath(emitterId, w);
  // No window: send nothing. An invented window succeeds and returns zero rows, and zero rows
  // renders exactly like a signal that decoded nothing.
  if (path === null || w === null) return { kind: "no-window" };
  let decodes: readonly DecodeRow[];
  try {
    decodes = (await client.get<DecodeResponse>(path)).decodes;
  } catch (e) {
    return { kind: "error", message: apiErrorText(e) };
  }
  if (decodes.length > 0) return { kind: "rows", decodes };
  return { kind: "empty", coverage: await windowCoverage(client, state, w) };
}

/** The three `rds.recipe.json` `messages` outputs' `frame_model`s (recipe file, `outputs[].decode.
 * frame_model`): `group-info` → `rds-group` (PI/TP/PTY, the identity sighting), `station` →
 * `rds-ps` (assembled 8-char PS), `radiotext` → `rds-rt` (assembled 64-char RadioText). Each is
 * its own `/api/inventory/{id}/decode` row (T-252 scope: this built-in decode path only, not the
 * separate `hk-rds` plugin's differently-shaped `rds-pi` frame). */
const RDS_GROUP_FRAME = "rds-group", RDS_PS_FRAME = "rds-ps", RDS_RT_FRAME = "rds-rt";

export interface RdsViewModel {
  /** Assembled Programme Service name (8 chars, space-padded), or `null`: `station`'s `text`
   * field has never committed a row for this emitter (`rds.recipe.json`'s `text` block only emits
   * once every segment up to the end of the string has arrived — never a partial string, per its
   * `on-complete` default; T-252 "never a fabricated value" holds trivially here). */
  ps: string | null;
  /** Assembled RadioText (up to 64 chars), or `null`: same "never partial" rule via `radiotext`'s
   * `reset_on: radiotext.ab` A/B switch. */
  rt: string | null;
  /** Traffic Programme flag from the latest `group-info` row, or `null`: not yet received. */
  tp: boolean | null;
  /** Programme Type code from the latest `group-info` row, or `null`: not yet received. */
  pty: number | null;
  updatedAtS: number;
}

const str = (v: unknown): string | null => (typeof v === "string" ? v : null);
const num = (v: unknown): number | null => (typeof v === "number" ? v : null);
const bool = (v: unknown): boolean | null => (typeof v === "boolean" ? v : null);

/**
 * Merges the RDS recipe's three decode rows (group-info/station/radiotext, newest-first per
 * docs/api.md's `GET .../decode`) into one view model. `null` when none of the three has ever
 * produced a row for this emitter (the panel's own quiet empty state). Every value is read from
 * exactly the field the recipe's mapping commits under that frame model — `station`/`radiotext`
 * content is keyed `text` (the mapping's only content path, `ps.text`/`radiotext.text`, keyed by
 * its last segment per `hk_pipeline::recipes::messages::keyed`), `group-info` metadata is keyed
 * `tp`/`pty` verbatim (`rds.recipe.json`'s `metadata: ["group_type","version","tp","pty"]`) — this
 * is not parsing, just reading the named field the recipe declared. PI is deliberately not read
 * here: the recipe uses it only as the row's *identity* (`decode.identity.field`), never as a
 * mapped `metadata`/`content` path, so it never reaches a row's `fields` at all — see
 * [[rdsIdentity]], which reads it from the emitter's own `identity_value` instead (an API gap,
 * reported rather than derived: T-252).
 */
export function rdsViewModel(decodes: readonly DecodeRow[]): RdsViewModel | null {
  const rows = decodes.filter(
    (d) => d.frame_model === RDS_GROUP_FRAME || d.frame_model === RDS_PS_FRAME || d.frame_model === RDS_RT_FRAME,
  );
  if (rows.length === 0) return null;
  let ps: string | null = null, rt: string | null = null;
  let tp: boolean | null = null, pty: number | null = null;
  let updatedAtS = 0;
  for (const r of rows) {
    updatedAtS = Math.max(updatedAtS, r.at);
    if (r.frame_model === RDS_PS_FRAME) ps ??= str(r.fields.text);
    else if (r.frame_model === RDS_RT_FRAME) rt ??= str(r.fields.text);
    else {
      tp ??= bool(r.fields.tp);
      pty ??= num(r.fields.pty);
    }
  }
  return { ps, rt, tp, pty, updatedAtS };
}

/** The emitter's RDS PI, read from its own inventory row (`ui/src/inventory.ts`'s `Row`) rather
 * than `/decode` — `rds.recipe.json` only ever uses `pi`/`*.key` as the decode `identity` field,
 * which `/api/inventory/{id}/decode` does not expose in `fields` (see [[rdsViewModel]]'s doc); the
 * identity sighting the same decode produces does reach the emitter row as `identity_value`,
 * gated by the row's own `withheld`. `value` is `null` both before any RDS identity has been seen
 * for this emitter and while `withheld` is `true` — callers must show those two states
 * differently, never collapse them (T-207/T-164: absence is not a value). Guarded on
 * `identity_scheme === "rds-pi"` so a differently-identified emitter (or one not yet identified at
 * all) never borrows an unrelated identity as if it were a PI. */
export interface RdsIdentity { value: string | null; withheld: boolean }

export function rdsIdentity(row: Pick<Row, "identity_scheme" | "identity_value" | "withheld"> | undefined): RdsIdentity {
  if (!row || row.identity_scheme !== "rds-pi") return { value: null, withheld: false };
  return { value: row.identity_value ?? null, withheld: row.withheld };
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
export const RDS_PANEL_CLASS = "out-panel out-rds";

/** The widget class a panel source mounts (chosen by output kind, ADR-0015 §12.7). */
export const panelClass = (s: PanelSource): string =>
  (s.kind === "rds" ? RDS_PANEL_CLASS : s.kind === "digital" ? DIGITAL_PANEL_CLASS : AUDIO_PANEL_CLASS);

/** A field that arrives 2 (PS) or 4 (RadioText) characters at a time but only ever reaches
 * `/decode` once fully assembled (see [[rdsViewModel]]): `raw === null` is the true "not yet
 * received" state (T-207/T-164 — never rendered as blank or a placeholder value); a non-null raw
 * value that trims to nothing is a real received value that happens to be blank/all-padding, kept
 * visibly distinct so it never reads as "not yet received" either. */
export function rdsFieldText(raw: string | null): string {
  if (raw === null) return "not yet received";
  return trimRds(raw) ?? "(blank)";
}

/** Renders the accumulated RDS readout (T-252) into `el`: PS, RadioText, PI, PTY and TP as the
 * recipe's rows already report them, plus a Clock (CT) row that is never populated — `rds.recipe.
 * json` has no group-4A node, so CT is a recipe/API gap (reported, not derived; see the task's
 * final report) rather than a field this code could ever fill in. Shared by the dedicated RDS
 * panel and the compact box under a Listen scope, so the two never disagree. */
function renderRdsBox(el: HTMLElement, view: DecodeView, pi: RdsIdentity): void {
  // T-384: an empty panel names *which* emptiness it is, exactly as the sidebar does. "No RDS
  // decode yet" read the same whether the station had decoded nothing in the viewed window, the
  // receiver had never looked there, or — the bug — no window had been asked about at all.
  if (view.kind !== "rows") {
    el.replaceChildren(h("p", { class: "hint" }, decodeEmptyText(view)));
    return;
  }
  const rds = rdsViewModel(view.decodes);
  const v = rds ?? { ps: null, rt: null, tp: null, pty: null, updatedAtS: 0 };
  const hasAnything = v.ps !== null || v.rt !== null || v.tp !== null || v.pty !== null || pi.value !== null || pi.withheld;
  if (!hasAnything) {
    // Rows came back for the window, but none of them is RDS: a statement about this decode, not
    // about the window, so it keeps its own sentence.
    el.replaceChildren(h("p", { class: "hint" }, "No RDS decode in this window."));
    return;
  }
  const piText = pi.withheld ? "withheld" : (pi.value ?? "not yet received");
  el.replaceChildren(
    h("dl", { class: "kv" },
      h("dt", {}, "Station"), h("dd", {}, rdsFieldText(v.ps)),
      h("dt", {}, "RadioText"), h("dd", {}, rdsFieldText(v.rt)),
      h("dt", {}, "PI"), h("dd", { class: "mono" }, piText),
      h("dt", {}, "Programme type"), h("dd", {}, v.pty === null ? "not yet received" : String(v.pty)),
      h("dt", {}, "Traffic programme"), h("dd", {}, v.tp === null ? "not yet received" : (v.tp ? "yes" : "no")),
      h("dt", {}, "Clock (CT)"), h("dd", { class: "hint" }, "not decoded by this recipe"),
    ),
  );
}

class DigitalPanel {
  private cleanup: (() => void) | null = null;
  constructor(el: HTMLElement, ctx: AppContext, pipelineId: string) {
    this.cleanup = mountInspectorPanel(el, ctx, pipelineId);
  }
  destroy() { this.cleanup?.(); this.cleanup = null; }
}

/** The dedicated RDS panel (T-252): polls the same `/decode` route as everything else here and
 * renders the accumulated view, independent of whether a raw packet-inspector output exists for
 * this pipeline — that's exactly the point, see [[collectPanelSources]]. */
class RdsPanel {
  private bodyEl: HTMLElement;
  private stopPoll: () => void;
  private unsubWindow: () => void;

  constructor(el: HTMLElement, private ctx: AppContext, private emitterId: string) {
    this.bodyEl = h("div", { class: "rds-box" }, h("p", { class: "hint" }, "Loading…"));
    el.replaceChildren(h("div", { class: "section-h" }, "RDS"), this.bodyEl);
    this.stopPoll = startPoll(() => this.load(), 3000);
    // Scrubbing must re-derive the panel, not leave the live edge's fields on screen under a past
    // window's heading (T-384). The 3 s poll would eventually catch up; a subscription makes the
    // panel move *with* the cursor instead of trailing it.
    this.unsubWindow = ctx.store.select(windowKey, () => { void this.load(); });
  }

  private async load() {
    const view = await loadDecodeView(this.ctx.client, this.ctx.store.get(), this.emitterId);
    const row = this.ctx.store.get().inventory.rows[this.emitterId];
    renderRdsBox(this.bodyEl, view, rdsIdentity(row));
  }

  destroy() { this.stopPoll(); this.unsubWindow(); }
}

class AudioPanel {
  private scope: ScopeBuffer = new ScopeBuffer(SCOPE_CAPACITY);
  private svg: SVGSVGElement;
  private poly: SVGPolylineElement;
  private rdsEl: HTMLElement;
  private unsubSamples: () => void;
  private stopPoll: () => void = () => {};
  private unsubWindow: () => void = () => {};
  private hasRds = false;
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
    this.rdsEl = h("div", { class: "rds-box", hidden: true }, h("p", { class: "hint" }, "Loading…"));
    el.replaceChildren(
      h("div", { class: "out-scope" }, this.svg),
      this.rdsEl,
    );
    this.unsubSamples = getAudioSession(ctx).onSamples(outputId, (pcm) => { this.scope.push(pcm); this.scheduleDraw(); });
  }

  /** Re-derives the panel's capabilities from the served output state on EVERY update, never once at
   * build time: the RDS box shows (and polls) exactly while a running RDS sibling output exists. */
  update(source: Extract<PanelSource, { kind: "audio" }>) {
    const hasRds = source.rdsSibling !== null;
    if (hasRds === this.hasRds) return;
    this.hasRds = hasRds;
    this.rdsEl.hidden = !hasRds;
    this.stopPoll();
    this.unsubWindow();
    this.stopPoll = () => {};
    this.unsubWindow = () => {};
    if (hasRds) {
      this.stopPoll = startPoll(() => this.loadDecode(), 3000);
      this.unsubWindow = this.ctx.store.select(windowKey, () => { void this.loadDecode(); });
    }
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
    const view = await loadDecodeView(this.ctx.client, this.ctx.store.get(), this.emitterId);
    const row = this.ctx.store.get().inventory.rows[this.emitterId];
    renderRdsBox(this.rdsEl, view, rdsIdentity(row));
  }

  destroy() {
    cancelAnimationFrame(this.raf);
    this.unsubSamples();
    this.stopPoll();
    this.unsubWindow();
  }
}

type MountedPanel = { destroy(): void };

class OutputPanels {
  private sources: PanelSource[] = [];
  private active: string | null = null;
  private mounted: MountedPanel | null = null;
  private mountedKey: string | null = null;

  private tabsEl: HTMLElement;
  private bodyEl: HTMLElement;

  constructor(el: HTMLElement, private ctx: AppContext) {
    this.tabsEl = h("div", { class: "out-tabs", role: "tablist" });
    this.bodyEl = h("div", { class: "out-body" });
    const analyzeEl = h("div", { class: "out-analyze", hidden: true });
    const traceEl = h("div", { class: "out-trace", hidden: true });
    el.replaceChildren(h("div", { class: "out-panels" }, analyzeEl, traceEl, h("div", { class: "section-h" }, "Outputs"), this.tabsEl, this.bodyEl));
    const trace = mountTracePanel(traceEl, ctx);
    mountAnalyzeSection(analyzeEl, ctx, (j) => trace.setJob(j));
    // LP-7: the same two slices the map's box badges read (`centre/surface.ts` `boxActivity`), so
    // the panel and the badge are two views of one output model and cannot disagree.
    ctx.store.select((s) => [s.outputs, s.servedOutputs] as const, () => this.recompute(), { immediate: true });
  }

  private recompute() {
    this.sources = panelSourcesOf(this.ctx.store.get());
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
    const key = panelKey(current);
    if (key !== this.mountedKey) {
      this.mounted?.destroy();
      this.mounted = null;
      this.mountedKey = key;
      this.bodyEl.replaceChildren();
      if (current) {
        const panelEl = h("div", { class: panelClass(current) });
        this.bodyEl.append(panelEl);
        if (current.kind === "audio") {
          const audio = new AudioPanel(panelEl, this.ctx, current.emitterId, current.outputId);
          audio.update(current);
          this.mounted = audio;
        } else {
          this.mounted = current.kind === "rds"
            ? new RdsPanel(panelEl, this.ctx, current.emitterId)
            : new DigitalPanel(panelEl, this.ctx, current.pipelineId);
        }
      }
    } else if (current?.kind === "audio" && this.mounted instanceof AudioPanel) {
      this.mounted.update(current);
    }
    const empty = panelsEmptyText(this.sources);
    if (empty && !this.bodyEl.hasChildNodes()) this.bodyEl.replaceChildren(h("div", { class: "empty" }, empty));
  }
}

export const mountOutputPanels: MountFn = (el, ctx) => { new OutputPanels(el, ctx); };
