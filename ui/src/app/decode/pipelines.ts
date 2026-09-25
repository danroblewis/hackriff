// Decode workbench: pipelines list, "New from recipe", recipe stage list and blocks palette
// (ADR-0013 §4.6, §8). Owner: T-153. Mounts the whole `wb-side` into the `pipelines` slot. Also
// exports the pipeline/recipe/block types and a small shared poller (`subscribeDecodeFeed`) that
// `stages.ts`, `plots.ts` and `params.ts` reuse instead of each polling `/api/pipelines` on its own.
import type { ControlClient } from "../../controls/client";
import type { AppContext, MountFn } from "../context";
import { h } from "../dom";
import { PIPELINES_SUBJECT, liveOnlyNote } from "../live-only";
import { startPoll } from "../net";
import { setMode, toast } from "../state";
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

interface FeedConn {
  feed: DecodeFeed; subs: Set<(f: DecodeFeed) => void>; stopPoll: () => void;
  /** Ticks once per poll request and once per [[publishPipeline]], so each is ordered against the other. */
  seq: number;
  /** Pipelines this tab created, stamped with the `seq` at which the create call returned (T-944). */
  created: Map<string, { p: Pipeline; seq: number }>;
}
const feedRegistry = new WeakMap<AppContext, FeedConn>();
/** Created pipelines published while no decode panel was subscribed; seed the next feed (T-944). */
const seedRegistry = new WeakMap<AppContext, Map<string, Pipeline>>();

/**
 * T-944: the pipelines list a poll answer implies, given what this tab has created. `GET
 * /api/pipelines` lists a pipeline from the moment `POST /api/pipelines` returns (the runtime
 * registers it before answering), but a poll **sent before** that create returned can still arrive
 * after it — that stale `[]` is what made a just-started pipeline vanish ("no pipelines running").
 * So a pipeline created after the poll was asked (`created.seq > askedSeq`) and missing from its
 * answer is kept; one created before it was asked is the server's to report (a poll asked after
 * the create returned is authoritative, e.g. for a stop). The server's copy always wins over ours.
 */
export function mergeCreated(
  answer: readonly Pipeline[], created: ReadonlyMap<string, { p: Pipeline; seq: number }>, askedSeq: number,
): Pipeline[] {
  const out = [...answer];
  for (const { p, seq } of created.values()) {
    if (seq > askedSeq && !out.some((x) => x.id === p.id)) out.push(p);
  }
  return out;
}

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
    const seed = seedRegistry.get(ctx);
    seedRegistry.delete(ctx);
    const c: FeedConn = {
      feed: { pipelines: seed ? [...seed.values()] : [], recipes: [], blocks: [], error: null },
      subs: new Set(), stopPoll: () => {}, seq: 0,
      // Seeded at seq 0: the first poll is asked after those creates returned, so it is authoritative.
      created: new Map([...(seed?.values() ?? [])].map((p) => [p.id, { p, seq: 0 }])),
    };
    c.stopPoll = startPoll(
      async () => {
        const asked = ++c.seq;
        const r = await ctx.client.get<{ pipelines: Pipeline[] }>("/api/pipelines");
        for (const [id, e] of c.created) if (e.seq < asked) c.created.delete(id);
        c.feed = { ...c.feed, pipelines: mergeCreated(r.pipelines, c.created, asked), error: null };
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

/** T-944: shows a pipeline `POST /api/pipelines` just answered with, at once and in the state the
 * server gave, instead of waiting up to a poll interval (or losing it to a poll already in flight). */
export function publishPipeline(ctx: AppContext, p: Pipeline): void {
  const c = feedRegistry.get(ctx);
  if (!c) {
    const seed = seedRegistry.get(ctx) ?? new Map<string, Pipeline>();
    seed.set(p.id, p);
    seedRegistry.set(ctx, seed);
    return;
  }
  c.created.set(p.id, { p, seq: ++c.seq });
  c.feed = { ...c.feed, pipelines: [...c.feed.pipelines.filter((x) => x.id !== p.id), p] };
  for (const s of c.subs) s(c.feed);
}

/** The toast after a start: which pipeline, in which state, on what (T-944: the user sees that
 * something started, from the server's own answer). */
export function startedText(name: string, p: Pipeline, on: string): string {
  return `${name}: ${p.id} ${pipelineStatusChip(p).text} on ${on}`;
}

/** `POST /api/pipelines` for `recipe_id` (and `version`) on `target`, published into the feed and
 * selected in the workbench the moment it returns. Throws the call's error. */
export async function startPipeline(
  ctx: AppContext, body: { recipe_id: string; version?: number; target: Record<string, unknown> },
): Promise<Pipeline> {
  const p = await ctx.client.post<Pipeline>("/api/pipelines", body);
  publishPipeline(ctx, p);
  ctx.store.set(selectPipeline(p.id));
  return p;
}

/** One ranked candidate from `GET /api/recipes/match` (docs/api.md "Recipe matching"). */
export interface RecipeMatch { id: string; version: number; name: string; score: number; outcome: "fit" | "partial" | "none" }
export type AutoDecode =
  | { ok: true; pipeline: Pipeline; recipe: RecipeMatch }
  | { ok: false; reason: "no_match" | "failed"; message: string };

/**
 * T-944: a signal's default Decode action. The backend ranks every recipe against what was
 * **measured** on the emitter (`GET /api/recipes/match`, best first; empty when nothing clears its
 * floor) — the UI does no matching of its own, it starts the backend's first choice on the emitter.
 * Nothing offered → `no_match`, and the recipe list in the Decode tab is the override.
 */
export async function autoDecode(ctx: AppContext, emitterId: string): Promise<AutoDecode> {
  let best: RecipeMatch | undefined;
  try {
    const m = await ctx.client.get<{ recipes: RecipeMatch[] }>(`/api/recipes/match?emitter=${encodeURIComponent(emitterId)}`);
    best = m.recipes[0];
  } catch (e) {
    return { ok: false, reason: "failed", message: `recipe match: ${e instanceof Error ? e.message : String(e)}` };
  }
  if (!best) return { ok: false, reason: "no_match", message: "no recipe fits what was measured — pick one from the recipe list" };
  try {
    const pipeline = await startPipeline(ctx, { recipe_id: best.id, version: best.version, target: { emitter_id: emitterId } });
    return { ok: true, pipeline, recipe: best };
  } catch (e) {
    return { ok: false, reason: "failed", message: `${best.name}: ${e instanceof Error ? e.message : String(e)}` };
  }
}

/** [[autoDecode]] with its outcome toasted and the Decode tab opened on the result (T-944). */
export async function runAutoDecode(ctx: AppContext, emitterId: string): Promise<AutoDecode> {
  const res = await autoDecode(ctx, emitterId);
  ctx.store.set(setMode("decode"));
  ctx.store.set(toast(res.ok ? startedText(res.recipe.name, res.pipeline, "the signal (best match)") : `Decode: ${res.message}`));
  return res;
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
      const p = await startPipeline(ctx, { recipe_id: r.id, target: target.target });
      ctx.store.set(toast(startedText(r.name, p, target.label)));
    } catch (e) {
      ctx.store.set(toast(`${r.name}: ${e instanceof Error ? e.message : String(e)}`));
    }
  };
  // T-944: with a signal focused, the entry point is the backend's best-matching recipe; the list
  // after it is the override.
  const focus = ctx.store.get().focus;
  const auto = focus.kind === "signal" && focus.id
    ? h("button", { class: "mini primary", type: "button", title: "the recipe the server ranks best against this signal's measurements", onclick: () => void runAutoDecode(ctx, focus.id as string) }, "Decode focused signal")
    : null;
  return h("div", { class: "new-row" },
    auto,
    h("span", { style: "color:var(--dim);font-size:11px;align-self:center" }, auto ? "or pick a recipe:" : "Start a recipe:"),
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
