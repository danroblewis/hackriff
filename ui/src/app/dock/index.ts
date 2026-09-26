// Active outputs (ADR-0013 §8, §4.8; T-150, reshaped by T-994). Every stream this page opened —
// Listen audio and streamed pipeline records — with a meter/rate, Mute, Copy address and Stop.
//
// T-994 RETIRED THE DOCK BAR. The user (2026-09-25): "it might make more sense to enhance what's
// rendered on the waterfall map" — so a box with an open output says so ON THE MAP (the active halo
// and corner badge, `surface/marks.ts` / `surface/pins.ts`), and its actions live in the box's
// right-click / long-press menu (`menu/actions.ts`). What is left here is the secondary strip the
// ticket allows: a SMALL, CLOSEABLE chips row that exists only while something is open (no fixed
// bar, no reserved pixels when nothing is), so every former dock action — Mute, Copy address, Stop,
// Open — stays reachable, including for a band/selection Listen that has no box of its own.
//
// It also owns the one poll the map's badges read: `GET /api/pipelines` (decode) and
// `GET /api/outputs` (recordings) — the backend's open-output records (see `./activity.ts`).
import type { AppContext, AreaMounts } from "../context";
import { h } from "../dom";
import { OUTPUTS_SUBJECT, liveOnlyNote } from "../live-only";
import { dismissOutputsStrip, outputsStripShown, setMode, setServedOutputs, toast, type OutputEntry } from "../state";
import type { ServedPipeline, ServedRecording } from "./activity";
import { getAudioSession, setOutputsRefresher, stopOutput } from "./api";
import { copyAddressText, levelPct, outputsCountText } from "./outputs";

/** How often the open-output records are re-read (ms): a started decode or recording is badged
 * within this, and an ended one loses its badge within it. */
export const SERVED_OUTPUTS_POLL_MS = 1000;

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
    ...Array.from({ length: 5 }, () => h("i", { style: `height:${pct}%` })));
}

function chipEl(ctx: AppContext, o: OutputEntry): HTMLElement {
  const vis = o.kind === "audio" ? meter(o) : h("span", { class: "rate" }, o.recordsPerS === null ? "–" : `${o.recordsPerS.toFixed(1)} rec/s`);
  const muteOrOpen = o.kind === "audio"
    ? h("button", {
        class: "mini", type: "button", "aria-pressed": String(o.muted), "data-action": "mute",
        onclick: () => {
          getAudioSession(ctx).setMuted(o.id, !o.muted);
          ctx.store.set((s) => ({ outputs: s.outputs.map((e) => (e.id === o.id ? { ...e, muted: !e.muted } : e)) }));
        },
      }, o.muted ? "Unmute" : "Mute")
    : h("button", { class: "mini", type: "button", "data-action": "open", onclick: () => ctx.store.set(setMode("decode")) }, "Open");
  return h("div", { class: `out${o.muted ? " muted" : ""}${o.kind === "records" ? " pipe-out" : ""}`, "data-id": o.id, "data-state": o.state, role: "group", "aria-label": o.label },
    h("span", { class: "t" }, h("span", { class: `kind-dot ${o.kind === "audio" ? "audio" : "bits"}`, "aria-hidden": "true" }), o.label),
    h("div", { class: "vis" }, vis),
    h("div", { class: "ctl" },
      muteOrOpen,
      h("button", { class: "mini", type: "button", "data-action": "copy", onclick: () => copyAddress(ctx, o.tcpTarget) }, "Copy address"),
      h("button", { class: "mini del", type: "button", "data-action": "stop", onclick: () => stopOutput(ctx, o.id) }, "Stop")),
    h("span", { class: "k" }, o.message ?? o.sub));
}

function mount(el: HTMLElement, ctx: AppContext) {
  // T-387: **live-only, and it says so** — a socket this browser tab holds cannot exist in a window
  // an hour ago. The note rides the strip's title line, louder once the view is scrubbed back.
  const note = h("small", { class: "live-only" }, liveOnlyNote(OUTPUTS_SUBJECT, !ctx.store.get().time.live));
  const count = h("small", { class: "count" }, "0 live");
  const close = h("button", {
    class: "out-close", type: "button", "aria-label": "Close Active outputs (reopens when another output starts)",
    title: "Close — the map's boxes still show what is active; right-click one to stop it",
    onclick: () => ctx.store.set(dismissOutputsStrip),
  }, "×");
  const label = h("div", { class: "out-label" }, h("b", {}, "Active outputs"), count, note);
  const list = h("div", { class: "outs" });
  el.replaceChildren(label, list, close);

  ctx.store.select((s) => s.time.live, (live) => { note.textContent = liveOnlyNote(OUTPUTS_SUBJECT, !live); });

  ctx.store.select((s) => [s.outputs, s.outputsStripDismissed] as const, ([outputs]) => {
    count.textContent = outputsCountText(outputs);
    list.replaceChildren(...outputs.map((o) => chipEl(ctx, o)));
    el.hidden = !outputsStripShown(ctx.store.get());
  }, { immediate: true });

  // The open-output records (decode pipelines, recordings) the map's box badges are drawn from, and
  // the records rate (§4.8: GET /api/pipelines stats.frames delta ÷ Δt) for a streamed output.
  const prevFrames = new Map<string, { frames: number; tS: number }>();
  let timer = 0, inflight = false, again = false;
  const poll = async () => {
    if (inflight) { again = true; return; }
    inflight = true;
    window.clearTimeout(timer);
    try {
      const [pr, or] = await Promise.all([
        ctx.client.get<{ pipelines: (ServedPipeline & { stats?: { frames: number } })[] }>("/api/pipelines").catch(() => null),
        ctx.client.get<{ recordings: ServedRecording[] }>("/api/outputs").catch(() => null),
      ]);
      const prev = ctx.store.get().servedOutputs;
      const pipelines = pr ? pr.pipelines.map((p) => ({
        id: p.id, state: p.state, emitter_id: p.emitter_id ?? null,
        outputs: (p.outputs ?? []).map((o) => ({ id: o.id, kind: o.kind, stream_id: o.stream_id })),
      })) : prev.pipelines;
      const recordings = or ? (or.recordings ?? []).map((r) => ({ id: r.id, active: !!r.active, emitter_id: r.emitter_id ?? null })) : prev.recordings;
      if (JSON.stringify(pipelines) !== JSON.stringify(prev.pipelines) || JSON.stringify(recordings) !== JSON.stringify(prev.recordings)) {
        ctx.store.set(setServedOutputs({ pipelines, recordings }));
      }
      if (pr && ctx.store.get().outputs.some((o) => o.kind === "records")) {
        const now = Date.now() / 1000;
        const rates = new Map<string, number>();
        for (const p of pr.pipelines) {
          if (!p.stats) continue;
          const was = prevFrames.get(p.id);
          if (was && now > was.tS) rates.set(p.id, Math.max(0, (p.stats.frames - was.frames) / (now - was.tS)));
          prevFrames.set(p.id, { frames: p.stats.frames, tS: now });
        }
        if (rates.size > 0) {
          ctx.store.set((s) => ({
            outputs: s.outputs.map((o) => (o.kind === "records" && o.pipelineId && rates.has(o.pipelineId)
              ? { ...o, recordsPerS: rates.get(o.pipelineId)! } : o)),
          }));
        }
      }
    } finally {
      inflight = false;
      if (again) { again = false; void poll(); } else timer = window.setTimeout(() => void poll(), SERVED_OUTPUTS_POLL_MS);
    }
  };
  // A menu action that just opened or closed an output asks for the records now, so the badge does
  // not wait out a whole poll interval (it still only shows what the server then answers).
  setOutputsRefresher(() => { void poll(); });
  void poll();
}

export const mounts: AreaMounts = { outputs: mount };
