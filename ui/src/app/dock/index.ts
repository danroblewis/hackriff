// Outputs dock mount (ADR-0013 §8, §4.8). Owner: T-150. Every live stream this page opened —
// Listen audio and pipeline records — with a meter/rate, Mute, Copy address and Stop.
import type { AppContext, AreaMounts } from "../context";
import { h } from "../dom";
import { OUTPUTS_SUBJECT, liveOnlyNote } from "../live-only";
import { startPoll } from "../net";
import { toast, type OutputEntry } from "../state";
import { getAudioSession, stopOutput } from "./api";
import { copyAddressText, levelPct, outputsCountText } from "./outputs";

function copyAddress(ctx: AppContext, tcpTarget: string | null) {
  if (!tcpTarget) { ctx.store.set(toast("No address for this stream yet.")); return; }
  ctx.client.get<{ tcp: { addr: string } | null }>("/api/streams").then((r) => {
    if (!r.tcp) { ctx.store.set(toast("No TCP stream server on this run.")); return; }
    const text = copyAddressText(r.tcp.addr, tcpTarget);
    const done = () => ctx.store.set(toast("Copied address."));
    const failed = () => ctx.store.set(toast(text));
    if (navigator.clipboard?.writeText) navigator.clipboard.writeText(text).then(done, failed);
    else failed();
  }).catch(() => ctx.store.set(toast("Could not read the stream address.")));
}

function meter(o: OutputEntry): HTMLElement {
  const pct = levelPct(o.levelDbfs);
  return h("div", { class: "meter", "aria-hidden": "true" },
    ...Array.from({ length: 8 }, () => h("i", { style: `height:${pct}%` })));
}

function entryEl(ctx: AppContext, o: OutputEntry): HTMLElement {
  const vis = o.kind === "audio" ? meter(o) : h("span", { class: "rate" }, o.recordsPerS === null ? "–" : `${o.recordsPerS.toFixed(1)} rec/s`);
  const muteOrOpen = o.kind === "audio"
    ? h("button", {
        class: "mini", type: "button", "aria-pressed": String(o.muted),
        onclick: () => {
          getAudioSession(ctx).setMuted(o.id, !o.muted);
          ctx.store.set((s) => ({ outputs: s.outputs.map((e) => (e.id === o.id ? { ...e, muted: !e.muted } : e)) }));
        },
      }, o.muted ? "Unmute" : "Mute")
    : h("button", { class: "mini", type: "button", onclick: () => ctx.store.set(toast("Open the Decode view to see this pipeline.")) }, "Open");
  return h("div", { class: `out${o.muted ? " muted" : ""}${o.kind === "records" ? " pipe-out" : ""}`, "data-id": o.id },
    h("span", { class: "t" }, h("span", { class: `kind-dot ${o.kind === "audio" ? "audio" : "bits"}`, "aria-hidden": "true" }), o.label),
    h("div", { class: "vis" }, vis),
    h("div", { class: "ctl" },
      muteOrOpen,
      h("button", { class: "mini", type: "button", onclick: () => copyAddress(ctx, o.tcpTarget) }, "Copy address"),
      h("button", { class: "mini del", type: "button", onclick: () => stopOutput(ctx, o.id) }, "Stop")),
    h("span", { class: "k" }, o.message ?? o.sub));
}

function mount(el: HTMLElement, ctx: AppContext) {
  // T-387: **live-only, and it says so.** Every entry here is a stream *this page* has open, with a
  // Mute, a Stop and a Copy address — a socket this browser tab holds cannot exist in a window an
  // hour ago, and there would be nothing there to stop. The dock is session state, not a view of
  // the air; the note says which, and says it more plainly once the view is scrubbed back.
  const note = h("small", { class: "live-only" }, liveOnlyNote(OUTPUTS_SUBJECT, !ctx.store.get().time.live));
  const label = h("div", { class: "dock-label" }, h("b", {}, "Outputs"), h("small", {}, "0 live"), note);
  const list = h("div", { class: "outs" });
  el.replaceChildren(label, list);
  const count = label.querySelector("small")!;

  ctx.store.select((s) => s.time.live, (live) => { note.textContent = liveOnlyNote(OUTPUTS_SUBJECT, !live); });

  ctx.store.select((s) => s.outputs, (outputs) => {
    count.textContent = outputsCountText(outputs);
    list.replaceChildren(...(outputs.length
      ? outputs.map((o) => entryEl(ctx, o))
      : [h("div", { class: "empty" }, "Nothing streaming. Listen or run a decode pipeline to add one.")]));
  }, { immediate: true });

  // Records rate (§4.8: GET /api/pipelines, stats.frames delta ÷ Δt), only while a records entry
  // is on the dock.
  const prevFrames = new Map<string, { frames: number; tS: number }>();
  startPoll(async () => {
    if (!ctx.store.get().outputs.some((o) => o.kind === "records")) return;
    const r = await ctx.client.get<{ pipelines: { id: string; stats: { frames: number } }[] }>("/api/pipelines");
    const now = Date.now() / 1000;
    const rates = new Map<string, number>();
    for (const p of r.pipelines) {
      const prev = prevFrames.get(p.id);
      if (prev && now > prev.tS) rates.set(p.id, Math.max(0, (p.stats.frames - prev.frames) / (now - prev.tS)));
      prevFrames.set(p.id, { frames: p.stats.frames, tS: now });
    }
    if (rates.size === 0) return;
    ctx.store.set((s) => ({
      outputs: s.outputs.map((o) => (o.kind === "records" && o.pipelineId && rates.has(o.pipelineId)
        ? { ...o, recordsPerS: rates.get(o.pipelineId)! } : o)),
    }));
  }, 2000);
}

export const mounts: AreaMounts = { outputs: mount };
