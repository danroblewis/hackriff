// Decode workbench: the node-chain strip above the plots (ADR-0013 §4.6, §8, slot `stages`).
// Owner: T-153. Reuses `pipelines.ts` for the pipeline/recipe cache.
import type { MountFn } from "../context";
import { h } from "../dom";
import { STAGE_STATUS_SUBJECT, liveOnlyNote } from "../live-only";
import { findBlock, pipelineStatusChip, subscribeDecodeFeed, type BlockDescriptor, type DecodeFeed, type Pipeline, type PipelineNode } from "./pipelines";
import { selectNode } from "./slice";

export type ChipClass = "ok" | "warn" | "bad" | "";

/** A node's status chip from its `<node>.lock` / `.quality` / `.error_rate` keys, when present. */
export function nodeChip(status: Readonly<Record<string, number | string | boolean | null>>, nodeId: string): { text: string; cls: ChipClass } {
  const lock = status[`${nodeId}.lock`];
  const quality = status[`${nodeId}.quality`];
  const errorRate = status[`${nodeId}.error_rate`];
  if (lock === "locked") return { text: typeof quality === "number" ? `${(quality * 100).toFixed(0)}%` : "locked", cls: typeof quality === "number" && quality < 0.3 ? "warn" : "ok" };
  if (typeof lock === "string") return { text: lock, cls: lock === "unlocked" || lock === "lost" ? "bad" : "" };
  if (typeof errorRate === "number") return { text: `${(errorRate * 100).toFixed(1)}% err`, cls: errorRate > 0.1 ? "bad" : errorRate > 0.02 ? "warn" : "ok" };
  return { text: "—", cls: "" };
}

function nodeButton(n: PipelineNode, i: number, current: string | null, status: Pipeline["status"], onSelect: (id: string) => void): HTMLElement[] {
  const chip = nodeChip(status, n.id);
  const btn = h("button", { class: "node", type: "button", "aria-current": String(n.id === current), onclick: () => onSelect(n.id) },
    h("span", { class: "top" }, h("b", {}, n.id), h("span", { class: "n" }, String(i + 1))),
    h("small", { class: chip.cls === "bad" ? "fx-bad" : chip.cls === "warn" ? "fx-gt" : "" }, `${n.block} · ${chip.text}`),
  );
  return i ? [h("span", { class: "arrow", "aria-hidden": "true" }, "→"), btn] : [btn];
}

export const mountStages: MountFn = (el, ctx) => {
  let feed: DecodeFeed = { pipelines: [], recipes: [], blocks: [], error: null };

  const render = () => {
    const d = ctx.store.get().decode;
    const p = feed.pipelines.find((x) => x.id === d.pipelineId) ?? null;
    if (!p) { el.replaceChildren(h("div", { class: "empty" }, "select a pipeline")); return; }
    // If nothing is selected yet, focus the first (or last-locked) stage so plots/params have a node.
    if (d.nodeId === null && p.nodes.length) { ctx.store.set(selectNode(p.nodes[0].id)); return; }
    el.replaceChildren(
      ...p.nodes.flatMap((n, i) => nodeButton(n, i, d.nodeId, p.status, (id) => ctx.store.set(selectNode(id)))),
      // T-387: **live-only, and it says so.** These chips are the pipeline's `status` map as
      // `/api/pipelines` last served it — a readout of the decoder (lock, quality, error rate)
      // right now, not a measurement of the band over the window on screen. The `status` records
      // themselves are stored in the capture file (stream contract §14.7) but nothing indexes or
      // serves them by time, and nothing should read a lock from an hour ago as this stage's state.
      h("small", { class: "live-only" }, liveOnlyNote(STAGE_STATUS_SUBJECT, !ctx.store.get().time.live)),
    );
  };

  subscribeDecodeFeed(ctx, (f) => { feed = f; render(); });
  ctx.store.select((s) => s.decode, render);
  ctx.store.select((s) => s.time.live, render);
  render();
};

export function blockFor(feed: DecodeFeed, block: string): BlockDescriptor | null {
  return findBlock(feed.blocks, block);
}

export { pipelineStatusChip };
