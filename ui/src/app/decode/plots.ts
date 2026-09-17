// Decode workbench: the two per-stage plots (ADR-0013 §4.6, §8, slot `plots`). Owner: T-153.
// Renders arrays the API already serves (stage taps, decoded frame metadata) as SVG; computes no
// signal facts. The MPX/subcarrier spectrum (API gap 4), eye/timing diagrams (gap 5) and the
// sync-search plot (gap 6) aren't served yet, so those show an honest placeholder instead.
import type { AppContext, MountFn } from "../context";
import { h } from "../dom";
import { viewWindow, windowKey, type ViewWindow } from "../explore/inventory";
import { FLAG_GATED, openStream, type StreamSocket } from "../net";
import { captureFor, captureFramesPath, type CaptureLite } from "./captures";
import { findBlock, subscribeDecodeFeed, type BlockDescriptor, type DecodeFeed, type PipelineNode, type PortType } from "./pipelines";
import { bitStripPlot, linePlot, placeholderPlot, scatterPlot, tallyPlot } from "./svg";
import { type FrameRecord, subscribePipelineFeed } from "./status-feed";

// ---- plot selection (pure) ----

export type PlotChoice =
  | { kind: "tap"; port: string; portType: PortType; title: string; note: string; caption: string }
  | { kind: "frame-tally-crc"; title: string; note: string; caption: string }
  | { kind: "channel-map"; title: string; note: string; caption: string }
  | { kind: "gap"; gap: 4 | 5 | 6; title: string; note: string; caption: string };

function outPort(block: BlockDescriptor | null, type: PortType): PortSpecLike | null {
  if (!block) return null;
  return block.outputs.find((o) => o.types.includes(type)) ?? null;
}
interface PortSpecLike { name: string; types: readonly PortType[] }

/** Picks the two plots for a stage from its block's declared output ports. Pure: the same node
 * and block always yield the same choice. */
export function pickPlots(node: PipelineNode | null, block: BlockDescriptor | null, followHops: boolean): [PlotChoice, PlotChoice] {
  if (!node || !block) {
    return [
      { kind: "gap", gap: 4, title: "Stage plots", note: "", caption: "select a stage" },
      { kind: "gap", gap: 4, title: "Stage plots", note: "", caption: "select a stage" },
    ];
  }
  const frames = outPort(block, "frames");
  if (frames) {
    return [
      { kind: "frame-tally-crc", title: "Frames by CRC status", note: "this window", caption: "tally of decoded frames, this stage's output" },
      followHops
        ? { kind: "channel-map", title: "Frames by channel", note: "this window", caption: "which channel each frame arrived on" }
        : { kind: "gap", gap: 6, title: "Sync search", note: "", caption: "match score per candidate position — not served yet (API gap 6)" },
    ];
  }
  const soft = outPort(block, "soft");
  if (soft) {
    return [
      { kind: "tap", port: soft.name, portType: "soft", title: "Soft symbol values", note: soft.name, caption: `raw samples from the ${soft.name} port` },
      { kind: "gap", gap: 5, title: "Eye / timing diagram", note: "", caption: "clock-recovery diagnostic — not served yet (API gap 5)" },
    ];
  }
  const iq = outPort(block, "iq");
  if (iq) {
    return [
      { kind: "tap", port: iq.name, portType: "iq", title: "IQ samples", note: iq.name, caption: `raw complex samples from the ${iq.name} port, decimated for display` },
      { kind: "gap", gap: 4, title: "MPX / subcarrier spectrum", note: "", caption: "view=spectrum — not served yet (API gap 4)" },
    ];
  }
  const real = outPort(block, "real");
  if (real) {
    return [
      { kind: "tap", port: real.name, portType: "real", title: "Waveform", note: real.name, caption: `raw samples from the ${real.name} port` },
      { kind: "gap", gap: 4, title: "Spectrum", note: "", caption: "view=spectrum — not served yet (API gap 4)" },
    ];
  }
  const bits = outPort(block, "bits");
  if (bits) {
    return [
      { kind: "tap", port: bits.name, portType: "bits", title: "Bit strip", note: bits.name, caption: `raw bits from the ${bits.name} port` },
      { kind: "gap", gap: 5, title: "Eye / timing diagram", note: "", caption: "clock-recovery diagnostic — not served yet (API gap 5)" },
    ];
  }
  return [
    { kind: "gap", gap: 4, title: block.name, note: "", caption: "this block has no diagnostic port" },
    { kind: "gap", gap: 4, title: block.name, note: "", caption: "this block has no diagnostic port" },
  ];
}

// ---- binary stage-tap payload decoding (pure) ----

export interface BinaryRecord { tS: number; gated: boolean; payload: ArrayBuffer }

/** Parses one §5.2 binary record: 32-byte header, then whole elements of the stream's datatype. */
export function parseBinaryRecord(buf: ArrayBuffer): BinaryRecord | null {
  if (buf.byteLength < 32) return null;
  const dv = new DataView(buf);
  const type = dv.getUint8(0), flags = dv.getUint8(1);
  if (type !== 1) return null; // data records only; dropped/status markers carry nothing to plot
  const tS = Number(dv.getBigInt64(16, true) / 1000n) / 1e6;
  return { tS, gated: !!(flags & FLAG_GATED), payload: buf.slice(32) };
}

export function decodeIq(payload: ArrayBuffer): { x: number; y: number }[] {
  const f = new Float32Array(payload);
  const out: { x: number; y: number }[] = [];
  for (let i = 0; i + 1 < f.length; i += 2) out.push({ x: f[i], y: f[i + 1] });
  return out;
}
export function decodeReal(payload: ArrayBuffer): number[] { return Array.from(new Float32Array(payload)); }
export function decodeBits(payload: ArrayBuffer): number[] { return Array.from(new Uint8Array(payload)); }

/** Decimates to at most `max` points, evenly spaced (display only). */
export function decimate<T>(arr: readonly T[], max: number): T[] {
  if (arr.length <= max) return [...arr];
  const step = arr.length / max;
  const out: T[] = [];
  for (let i = 0; i < max; i++) out.push(arr[Math.floor(i * step)]);
  return out;
}

// ---- frame tallies (pure) ----

export function tallyCrcStatus(frames: readonly FrameRecord[]): { label: string; count: number }[] {
  const counts = new Map<string, number>();
  for (const f of frames) {
    const k = typeof f.crc_status === "string" ? f.crc_status : "unknown";
    counts.set(k, (counts.get(k) ?? 0) + 1);
  }
  return [...counts.entries()].map(([label, count]) => ({ label, count })).sort((a, b) => b.count - a.count);
}

export function tallyChannels(frames: readonly FrameRecord[]): { label: string; count: number }[] {
  const counts = new Map<string, number>();
  for (const f of frames) {
    const md = f.metadata as Record<string, unknown> | undefined;
    const hz = md && typeof md.channel_hz === "number" ? md.channel_hz : null;
    const k = hz !== null ? `${(hz / 1e6).toFixed(3)} MHz` : "unknown";
    counts.set(k, (counts.get(k) ?? 0) + 1);
  }
  return [...counts.entries()].map(([label, count]) => ({ label, count })).sort((a, b) => b.count - a.count);
}

/**
 * Keeps only the frames whose own `t_ns` (stream-contract §5.1) falls inside the view window,
 * which is `[t0, t1]` in Unix **seconds on the capture clock** (T-384).
 *
 * This used to be `withinWindow(frames, Date.now() * 1e6, 60)` — a wall-clock instant compared
 * against capture-clock stamps, the same bug T-379 removed from the inventory path and still live
 * here at three call sites. It is not an off-by-a-bit: a replay's clock and the browser's are
 * unrelated, and the fixture behind T-379 sat 3.5 days apart, so *every* frame fell outside the
 * 60 s window and the tally rendered permanently empty — or, with the offset the other way, every
 * frame fell inside it and the "last 60 s" tally silently became an all-time one.
 *
 * Both ends are closed, and both come from the window: a tally of only-since-then would show
 * frames the waterfall above it no longer displays.
 */
export function withinWindow(frames: readonly FrameRecord[], w: ViewWindow): FrameRecord[] {
  const t0 = w.t0 * 1e9, t1 = w.t1 * 1e9;
  return frames.filter((f) => typeof f.t_ns === "number" && f.t_ns >= t0 && f.t_ns <= t1);
}

/** Newest `cap` frames, by arrival. The live buffer is bounded by *count*, never by a clock: a
 * time-trimmed buffer would discard live frames while the view is scrubbed back, so going Live
 * again would find the plot empty of data it had already received (T-384). */
export function capFrames(frames: readonly FrameRecord[], cap: number): FrameRecord[] {
  return frames.length <= cap ? [...frames] : frames.slice(frames.length - cap);
}

// ---- the window's frames, when the live tap doesn't hold them (T-384) ----
//
// Moved to `./captures` by T-387 so the packet inspector can reach the same route through the same
// helpers; re-exported here because this module introduced them and its tests name them.
export { CAPTURE_FRAME_LIMIT, captureFor, captureFramesPath, type CaptureLite } from "./captures";

// ---- mount ----

const PLOT_W = 520, PLOT_H = 190, MAX_POINTS = 800;

/** How many live frames the tap keeps. Bounded by count, not by a clock — see [[capFrames]]. Large
 * enough to cover a long dragged span at a busy frame rate, small enough to re-tally per render. */
const FRAME_BUF_CAP = 4000;

function plotPanel(id: "a" | "b"): HTMLElement {
  return h("div", { class: "plot" },
    h("div", { class: "section-h" }, h("span", { "data-p": `${id}-title` }), h("em", { "data-p": `${id}-note` })),
    h("div", { "data-p": `${id}-body` }),
    h("div", { class: "cap", "data-p": `${id}-cap` }),
  );
}

function fill(panel: HTMLElement, choice: PlotChoice, svgEl: SVGElement) {
  (panel.querySelector('[data-p$="-title"]') as HTMLElement).textContent = choice.title;
  (panel.querySelector('[data-p$="-note"]') as HTMLElement).textContent = choice.note;
  (panel.querySelector('[data-p$="-cap"]') as HTMLElement).textContent = choice.caption;
  const body = panel.querySelector('[data-p$="-body"]') as HTMLElement;
  body.replaceChildren(svgEl);
}

export const mountPlots: MountFn = (el, ctx: AppContext) => {
  const panelA = plotPanel("a"), panelB = plotPanel("b");
  el.replaceChildren(panelA, panelB);

  let feed: DecodeFeed = { pipelines: [], recipes: [], blocks: [], error: null };
  let tap: StreamSocket | null = null;
  let tapBuf: number[] = [];
  let tapIq: { x: number; y: number }[] = [];
  let frameBuf: FrameRecord[] = [];
  /** Frames fetched from the pipeline's capture for a window the live tap never carried (T-384),
   * and the window they are of — so a stale set is never tallied under a new window. */
  let backfill: { w: ViewWindow; frames: FrameRecord[] } | null = null;
  let unsubFrames: (() => void) | null = null;
  let unsubWindow: (() => void) | null = null;
  let currentKey = "";
  let renderTimer: ReturnType<typeof setTimeout> | null = null;

  const scheduleRender = () => {
    if (renderTimer !== null) return;
    renderTimer = setTimeout(() => { renderTimer = null; renderNow(); }, 200);
  };

  function renderNow() {
    const d = ctx.store.get().decode;
    const p = feed.pipelines.find((x) => x.id === d.pipelineId) ?? null;
    const node = p ? p.nodes.find((n) => n.id === d.nodeId) ?? null : null;
    const block = node ? findBlock(feed.blocks, node.block) : null;
    const [a, b] = pickPlots(node, block, !!p?.follow_hops);
    fill(panelA, a, plotSvg(a));
    fill(panelB, b, plotSvg(b));
  }

  /** The frames of the current view window: what the live tap received, plus whatever the
   * pipeline's capture held for a window the tap never saw. `null` when no window can be named —
   * the plot then says so instead of tallying an unbounded buffer as if it were a window. */
  function windowFrames(): { w: ViewWindow; frames: FrameRecord[] } | null {
    const w = viewWindow(ctx.store.get());
    if (w === null) return null;
    const live = withinWindow(frameBuf, w);
    const stored = backfill && backfill.w.t0 === w.t0 && backfill.w.t1 === w.t1 ? backfill.frames : [];
    // De-duplicated on the frame's own time+sequence: a window straddling the live edge is served
    // by both sources, and a frame counted twice would overstate a tally.
    const seen = new Set(live.map((f) => `${f.t_ns}/${f.seq}`));
    return { w, frames: [...live, ...stored.filter((f) => !seen.has(`${f.t_ns}/${f.seq}`))] };
  }

  function plotSvg(choice: PlotChoice): SVGElement {
    switch (choice.kind) {
      case "tap":
        if (choice.portType === "iq") return scatterPlot(PLOT_W, PLOT_H, tapIq);
        if (choice.portType === "bits") return bitStripPlot(PLOT_W, PLOT_H, tapBuf);
        return linePlot(PLOT_W, PLOT_H, tapBuf);
      case "frame-tally-crc": {
        const wf = windowFrames();
        return wf === null
          ? placeholderPlot(PLOT_W, PLOT_H, ["waiting for the capture window…"])
          : tallyPlot(PLOT_W, PLOT_H, tallyCrcStatus(wf.frames));
      }
      case "channel-map": {
        const wf = windowFrames();
        return wf === null
          ? placeholderPlot(PLOT_W, PLOT_H, ["waiting for the capture window…"])
          : tallyPlot(PLOT_W, PLOT_H, tallyChannels(wf.frames));
      }
      case "gap":
        return placeholderPlot(PLOT_W, PLOT_H, [choice.caption]);
    }
  }

  /**
   * Fetches the window's frames from the pipeline's capture, when the live tap cannot have them.
   *
   * Best-effort and additive: a failure or a pipeline with no capture leaves the live frames alone
   * rather than emptying the plot. It never re-decodes — `/api/captures/{id}/frames` serves records
   * the pipeline already wrote, which is what the incremental-decode invariant asks for.
   */
  async function loadBackfill(pipelineId: string) {
    const w = viewWindow(ctx.store.get());
    if (w === null) { backfill = null; return; }
    if (backfill && backfill.w.t0 === w.t0 && backfill.w.t1 === w.t1) return;
    try {
      const list = await ctx.client.get<{ captures?: readonly CaptureLite[] }>("/api/captures");
      const cap = captureFor(list.captures ?? [], pipelineId);
      if (!cap) { backfill = { w, frames: [] }; return; }
      const page = await ctx.client.get<{ frames?: readonly FrameRecord[] }>(captureFramesPath(cap.id, w));
      backfill = { w, frames: [...(page.frames ?? [])] };
    } catch {
      backfill = { w, frames: [] };
    }
    scheduleRender();
  }

  function teardownTap() {
    tap?.close();
    tap = null;
    tapBuf = [];
    tapIq = [];
  }
  function teardownFrames() {
    unsubFrames?.();
    unsubFrames = null;
    unsubWindow?.();
    unsubWindow = null;
    frameBuf = [];
    backfill = null;
  }

  function ensureWiring() {
    const d = ctx.store.get().decode;
    const p = feed.pipelines.find((x) => x.id === d.pipelineId) ?? null;
    const node = p ? p.nodes.find((n) => n.id === d.nodeId) ?? null : null;
    const block = node ? findBlock(feed.blocks, node.block) : null;
    const [a, b] = pickPlots(node, block, !!p?.follow_hops);
    const key = `${p?.id ?? ""}:${node?.id ?? ""}`;
    if (key === currentKey) { renderNow(); return; }
    currentKey = key;
    teardownTap();
    teardownFrames();
    const tapChoice = [a, b].find((c): c is Extract<PlotChoice, { kind: "tap" }> => c.kind === "tap");
    if (tapChoice && p) {
      tap = openStream(`/ws/open/stage?pipeline=${encodeURIComponent(p.id)}&node=${encodeURIComponent(node!.id)}&port=${encodeURIComponent(tapChoice.port)}`, ctx.token, {
        onHeader() { /* geometry not needed for raw display */ },
        onRefused() { /* leave the plot empty; the caption still names the port */ },
        onBinary(buf) {
          const rec = parseBinaryRecord(buf);
          if (!rec || rec.gated) return;
          if (tapChoice.portType === "iq") { tapIq = decimate([...tapIq, ...decodeIq(rec.payload)], MAX_POINTS); }
          else if (tapChoice.portType === "bits") { tapBuf = decimate([...tapBuf, ...decodeBits(rec.payload)], 256); }
          else { tapBuf = decimate([...tapBuf, ...decodeReal(rec.payload)], MAX_POINTS); }
          scheduleRender();
        },
        onClose() { tap = null; },
      });
    }
    const needsFrames = [a, b].some((c) => c.kind === "frame-tally-crc" || c.kind === "channel-map");
    if (needsFrames && p) {
      const pipelineId = p.id;
      unsubFrames = subscribePipelineFeed(ctx, pipelineId, {
        frame(f) { frameBuf = capFrames([...frameBuf, f], FRAME_BUF_CAP); scheduleRender(); },
      });
      // T-384: the tallies are a view over the same window as the waterfall, so they re-derive when
      // it moves — and fetch the window's recorded frames when the live tap never carried them.
      unsubWindow = ctx.store.select(windowKey, () => { void loadBackfill(pipelineId); });
      void loadBackfill(pipelineId);
    }
    renderNow();
  }

  subscribeDecodeFeed(ctx, (f) => { feed = f; ensureWiring(); });
  ctx.store.select((s) => s.decode, ensureWiring);
  ensureWiring();
};
