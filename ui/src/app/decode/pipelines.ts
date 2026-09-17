// Decode workbench: pipelines list, "New from recipe", recipe stage list and blocks palette
// (ADR-0013 §4.6, §8). Owner: T-153. Mounts the whole `wb-side` into the `pipelines` slot. Also
// exports the pipeline/recipe/block types and a small shared poller (`subscribeDecodeFeed`) that
// `stages.ts`, `plots.ts` and `params.ts` reuse instead of each polling `/api/pipelines` on its own.
import type { ControlClient } from "../../controls/client";
import type { AppContext, MountFn } from "../context";
import { h } from "../dom";
import { PIPELINES_SUBJECT, liveOnlyNote } from "../live-only";
import { startPoll } from "../net";
import { toast } from "../state";
import { selectNode, selectPipeline } from "./slice";

// ---- wire types (docs/api.md "Recipes and pipelines") ----

export type PortType = "iq" | "real" | "soft" | "bits" | "frames";
export interface PortSpec { name: string; types: readonly PortType[]; diagnostic: boolean }
export interface ParamSchema {
  name: string; type: string; required: boolean; default?: unknown; hot: boolean; doc: string;
  min?: number; max?: number; unit?: string; values?: readonly string[]; max_len?: number; max_bits?: number;
}
export interface BlockDescriptor {
  name: string; version: number; group: string; doc: string;
  inputs: readonly PortSpec[]; outputs: readonly PortSpec[]; params: readonly ParamSchema[]; params_pinned: boolean;
}
export interface PipelineNode { id: string; block: string; outputs: readonly string[] }
export interface PipelineOutput { id: string; kind: "inspector" | "stage" | "messages"; stream_id: string }
export interface PipelineChannel { index: number; center_hz: number; bandwidth_hz: number }
export interface Pipeline {
  id: string; recipe_id: string; recipe_version: number; edit_rev: number;
  state: "running" | "ended"; end_reason: string | null;
  target: Readonly<Record<string, unknown>>;
  channel: { center_hz: number; bandwidth_hz: number; sample_rate_hz: number };
  content_class: string; emitter_id: string | null; started: number;
  nodes: readonly PipelineNode[]; outputs: readonly PipelineOutput[];
  status: Readonly<Record<string, number | string | boolean | null>>;
  stats: { samples: number; chunks: number; frames: number; gaps: number; discontinuities: number; skipped_samples: number; edits: number; status_ticks: number; decodes: number; decodes_dropped: number };
  warnings: readonly unknown[];
  follow_hops: { channels: readonly PipelineChannel[]; channel_source: string; channel_bandwidth_hz: number; max_channels: number } | null;
}
export interface RecipeSummary { id: string; name: string; version: number; versions: readonly number[]; builtin: boolean; builtin_version: number | null; description: string; match: unknown; input: { port: PortType } }
export interface RecipeNode { id: string; block: string; version?: number; label?: string; doc?: string; params: Readonly<Record<string, unknown>>; inputs?: Readonly<Record<string, string>> }
export interface RecipeDoc {
  schema: string; schema_version: number; id: string; version: number; name: string; description?: string;
  match?: unknown; input: { port: PortType }; nodes: readonly RecipeNode[];
  field_maps?: Readonly<Record<string, unknown>>; outputs?: readonly unknown[];
}

// ---- pure view-model helpers ----

/** Status pill text/class for the pipeline list ("running" / "hopping" / "ended: <reason>"). */
export function pipelineStatusChip(p: Pipeline): { text: string; cls: "run" | "hop" | "end" } {
  if (p.state === "ended") return { text: `ended${p.end_reason ? `: ${p.end_reason}` : ""}`, cls: "end" };
  if (p.follow_hops) return { text: "hopping", cls: "hop" };
  return { text: "running", cls: "run" };
}

/** "101.300 MHz · 1 channel" or, for a hopping pipeline, its channel list. */
export function pipelineChannelText(p: Pipeline): string {
  if (p.follow_hops && p.follow_hops.channels.length) {
    return p.follow_hops.channels.map((c) => (c.center_hz / 1e6).toFixed(3)).join(" / ") + " MHz";
  }
  return `${(p.channel.center_hz / 1e6).toFixed(3)} MHz · 1 channel`;
}

export function findNode(p: Pipeline, nodeId: string | null): PipelineNode | null {
  return nodeId ? p.nodes.find((n) => n.id === nodeId) ?? null : null;
}

export function findBlock(blocks: readonly BlockDescriptor[], name: string): BlockDescriptor | null {
  return blocks.find((b) => b.name === name) ?? null;
}

/** The pipeline's recorded/records-ready output to stream (an `inspector` or `messages` output). */
export function primaryOutput(p: Pipeline): PipelineOutput | null {
  return p.outputs.find((o) => o.kind === "inspector") ?? p.outputs.find((o) => o.kind === "messages") ?? p.outputs[0] ?? null;
}

/** What `POST /api/pipelines` targets, from already-known UI state (focus/selection/view band). */
export function resolveTarget(focus: { kind: string; id?: string }, liveView: { loHz: number; hiHz: number } | null): { target: Record<string, unknown>; label: string } | null {
  if (focus.kind === "signal" && focus.id) return { target: { emitter_id: focus.id }, label: "the focused signal" };
  if (focus.kind === "selection" && focus.id) return { target: { selection_id: focus.id }, label: "the focused selection" };
  if (liveView) return { target: { band: { f_lo: liveView.loHz, f_hi: liveView.hiHz } }, label: "the current view" };
  return null;
}

// ---- shared poll: pipelines (2s), recipes and blocks (once), shared across decode panels ----

export interface DecodeFeed { pipelines: readonly Pipeline[]; recipes: readonly RecipeSummary[]; blocks: readonly BlockDescriptor[]; error: string | null }

interface FeedConn { feed: DecodeFeed; subs: Set<(f: DecodeFeed) => void>; stopPoll: () => void }
const feedRegistry = new WeakMap<AppContext, FeedConn>();

async function loadStatic(client: ControlClient, conn: FeedConn) {
  try {
    const [recipes, blocks] = await Promise.all([
      client.get<{ recipes: Record<string, RecipeSummary> }>("/api/recipes"),
      client.get<{ blocks: BlockDescriptor[] }>("/api/blocks"),
    ]);
    conn.feed = { ...conn.feed, recipes: Object.values(recipes.recipes), blocks: blocks.blocks };
    for (const s of conn.subs) s(conn.feed);
  } catch {
    // Recipes/blocks are fetched once; a transient failure just leaves the palette/list empty
    // until the next subscriber remount. The 2 s pipelines poll still runs and reports its own errors.
  }
}

/** Subscribes to the shared pipelines/recipes/blocks cache for this context; the first subscriber
 * starts the poll, the last unsubscribe stops it. Calls `cb` immediately with the current feed. */
export function subscribeDecodeFeed(ctx: AppContext, cb: (f: DecodeFeed) => void): () => void {
  let conn = feedRegistry.get(ctx);
  if (!conn) {
    const c: FeedConn = { feed: { pipelines: [], recipes: [], blocks: [], error: null }, subs: new Set(), stopPoll: () => {} };
    c.stopPoll = startPoll(
      async () => {
        const r = await ctx.client.get<{ pipelines: Pipeline[] }>("/api/pipelines");
        c.feed = { ...c.feed, pipelines: r.pipelines, error: null };
        for (const s of c.subs) s(c.feed);
      },
      2000,
      (e) => {
        c.feed = { ...c.feed, error: e instanceof Error ? e.message : String(e) };
        for (const s of c.subs) s(c.feed);
      },
    );
    void loadStatic(ctx.client, c);
    conn = c;
    feedRegistry.set(ctx, conn);
  }
  conn.subs.add(cb);
  cb(conn.feed);
  return () => {
    const c = feedRegistry.get(ctx);
    if (!c) return;
    c.subs.delete(cb);
    if (c.subs.size === 0) { c.stopPoll(); feedRegistry.delete(ctx); }
  };
}

// ---- mount ----

function pipelineRow(p: Pipeline, current: string | null, ctx: AppContext): HTMLElement {
  const chip = pipelineStatusChip(p);
  return h("div", { class: "pl", tabindex: "0", "aria-current": String(p.id === current), onclick: () => ctx.store.set(selectPipeline(p.id)) },
    h("b", {}, p.recipe_id),
    h("span", { class: `st ${chip.cls}` }, chip.text),
    h("span", { class: "cx" }, chip.cls === "hop" ? "⇄ " : "• ", pipelineChannelText(p)),
  );
}

function newFromRecipeRow(feed: DecodeFeed, ctx: AppContext): HTMLElement {
  const builtins = feed.recipes.filter((r) => r.builtin);
  const start = async (r: RecipeSummary) => {
    const target = resolveTarget(ctx.store.get().focus, ctx.store.get().live.view);
    if (!target) { ctx.store.set(toast("focus a signal or selection, or view a band, before starting a recipe")); return; }
    try {
      const created = await ctx.client.post<{ id: string }>("/api/pipelines", { recipe_id: r.id, target: target.target });
      ctx.store.set(selectPipeline(created.id));
      ctx.store.set(toast(`${r.name}: started on ${target.label}`));
    } catch (e) {
      ctx.store.set(toast(`${r.name}: ${e instanceof Error ? e.message : String(e)}`));
    }
  };
  return h("div", { class: "new-row" },
    h("span", { style: "color:var(--dim);font-size:11px;align-self:center" }, "New from recipe:"),
    ...(builtins.length ? builtins.map((r) => h("button", { class: "mini", type: "button", onclick: () => void start(r) }, r.name)) : [h("span", { class: "empty" }, "no recipes served")]),
  );
}

function stageListRow(p: Pipeline | null, current: string | null, ctx: AppContext): HTMLElement {
  if (!p) return h("div", { class: "empty" }, "select a pipeline");
  return h("div", { class: "stage-list" }, ...p.nodes.map((n, i) =>
    h("div", { class: "stg", tabindex: "0", "aria-current": String(n.id === current), onclick: () => ctx.store.set(selectNode(n.id)) },
      h("span", { class: "n" }, String(i + 1)),
      h("span", {}, n.id),
      h("code", {}, n.block),
    )));
}

function blocksPalette(blocks: readonly BlockDescriptor[]): HTMLElement {
  return h("div", { style: "display:flex;flex-wrap:wrap;gap:4px" },
    ...(blocks.length ? blocks.map((b) => h("span", { class: "chip", title: b.doc }, b.name)) : [h("span", { class: "empty" }, "no blocks served")]));
}

export const mountPipelines: MountFn = (el, ctx) => {
  let feed: DecodeFeed = { pipelines: [], recipes: [], blocks: [], error: null };

  const render = () => {
    const d = ctx.store.get().decode;
    const current = feed.pipelines.find((p) => p.id === d.pipelineId) ?? null;
    el.replaceChildren(
      h("div", { class: "wb-block" },
        // T-387: this list is **live-only and says so**. `GET /api/pipelines` answers "which
        // decoder processes exist in this run"; a process is running or it is not, and it has no
        // past-window form to ask for. What made the old "decoders running now" inadequate is that
        // it reads as a description while the view beside it may be scrubbed an hour back — the
        // note now names the state the reader is actually in.
        h("div", { class: "h" }, "Pipelines ", h("em", { class: "live-only" }, liveOnlyNote(PIPELINES_SUBJECT, !ctx.store.get().time.live))),
        feed.pipelines.length
          ? h("div", {}, ...feed.pipelines.map((p) => pipelineRow(p, d.pipelineId, ctx)))
          : h("div", { class: "empty" }, feed.error ? `${feed.error}` : "no pipelines running"),
        newFromRecipeRow(feed, ctx),
      ),
      h("div", { class: "wb-block" },
        h("div", { class: "h" }, "Recipe ", h("em", {}, current ? `${current.recipe_id} · ${current.nodes.length} blocks` : "")),
        stageListRow(current, d.nodeId, ctx),
      ),
      h("div", { class: "wb-block" },
        h("div", { class: "h" }, "Blocks ", h("em", {}, "available to recipes")),
        blocksPalette(feed.blocks),
      ),
    );
  };

  subscribeDecodeFeed(ctx, (f) => { feed = f; render(); });
  ctx.store.select((s) => s.decode, render);
  // The live-only note gets louder the moment the view stops following the live edge, so it has to
  // re-render on Play/Pause as well as on the feed.
  ctx.store.select((s) => s.time.live, render);
  render();
};
