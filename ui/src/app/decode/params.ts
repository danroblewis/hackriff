// Decode workbench: the stage-parameters panel (ADR-0013 §4.6, §8, slot `params`) — step guide,
// block parameters with blind "Use" suggestions, live quality tiles, and Next/Save/Stream/Reset.
// Owner: T-153. All values come from `/api/recipes`, `/api/blocks` and `/api/assist/*`; nothing
// here computes a signal fact.
import { startRecordsOutput } from "../dock/api";
import type { MountFn } from "../context";
import { h } from "../dom";
import { toast } from "../state";
import {
  findBlock, primaryOutput, subscribeDecodeFeed,
  type BlockDescriptor, type DecodeFeed, type ParamSchema, type Pipeline, type PipelineNode, type RecipeDoc, type RecipeNode,
} from "./pipelines";
import { selectNode } from "./slice";
import { nodeChip } from "./stages";

// ---- pure helpers (unit tested) ----

export function nextNodeId(pipeline: Pipeline, currentNodeId: string | null): string | null {
  const i = pipeline.nodes.findIndex((n) => n.id === currentNodeId);
  if (i < 0 || i + 1 >= pipeline.nodes.length) return null;
  return pipeline.nodes[i + 1].id;
}

/** Deep-clones `recipe` with one node's `params[name]` set to `value`. Pure. */
export function applyParam(recipe: RecipeDoc, nodeId: string, name: string, value: unknown): RecipeDoc {
  return {
    ...recipe,
    nodes: recipe.nodes.map((n) => (n.id === nodeId ? { ...n, params: { ...n.params, [name]: value } } : n)),
  };
}

/** Deep-clones `recipe` with several params of one node merged in (an assist suggestion's fragment). Pure. */
export function applyFragment(recipe: RecipeDoc, nodeId: string, params: Readonly<Record<string, unknown>>): RecipeDoc {
  return {
    ...recipe,
    nodes: recipe.nodes.map((n) => (n.id === nodeId ? { ...n, params: { ...n.params, ...params } } : n)),
  };
}

/** Client-side format coercion only (numbers/booleans from text input); real validation is the
 * server's `/api/recipes/validate`. */
export function coerceParamValue(schema: ParamSchema, raw: string): { ok: true; value: unknown } | { ok: false; error: string } {
  switch (schema.type) {
    case "bool":
      if (raw === "true" || raw === "false") return { ok: true, value: raw === "true" };
      return { ok: false, error: "true or false" };
    case "int": {
      const n = Number(raw);
      if (!Number.isInteger(n)) return { ok: false, error: "not an integer" };
      return { ok: true, value: n };
    }
    case "float": {
      const n = Number(raw);
      if (!Number.isFinite(n)) return { ok: false, error: "not a number" };
      return { ok: true, value: n };
    }
    default:
      return { ok: true, value: raw };
  }
}

export interface Tile { label: string; value: string; cls: "good" | "mid" | "bad" | "" }

/** Records/s from two `stats.frames` samples a `Δt` seconds apart. */
export function recordsPerSecond(prevFrames: number, prevAtS: number, curFrames: number, curAtS: number): number | null {
  const dt = curAtS - prevAtS;
  if (dt <= 0 || curFrames < prevFrames) return null;
  return (curFrames - prevFrames) / dt;
}

export function qualityTiles(pipeline: Pipeline, nodeId: string | null, recordsRate: number | null): Tile[] {
  const chip = nodeId ? nodeChip(pipeline.status, nodeId) : { text: "—", cls: "" as const };
  const errKey = nodeId ? pipeline.status[`${nodeId}.error_rate`] : null;
  const passRate = typeof errKey === "number" ? (1 - errKey) * 100 : null;
  return [
    { label: "Check pass rate", value: passRate === null ? "—" : `${passRate.toFixed(1)}%`, cls: passRate === null ? "" : passRate >= 90 ? "good" : passRate >= 50 ? "mid" : "bad" },
    { label: "Records / s", value: recordsRate === null ? "—" : recordsRate.toFixed(1), cls: recordsRate === null ? "" : recordsRate >= 1 ? "good" : "mid" },
    { label: "Lock", value: chip.text, cls: chip.cls === "" ? "" : chip.cls === "ok" ? "good" : chip.cls === "warn" ? "mid" : "bad" },
  ];
}

/** The block names whose parameters `/api/assist/*` can suggest from decoded frames (T-091). */
export function assistRouteFor(blockName: string): "/api/assist/sync" | "/api/assist/crc" | "/api/assist/fields" | null {
  if (blockName === "sync_search") return "/api/assist/sync";
  if (blockName === "crc" || blockName === "bch") return "/api/assist/crc";
  if (blockName === "fields") return "/api/assist/fields";
  return null;
}

// ---- mount ----

interface AssistFragment { block: string; params: Readonly<Record<string, unknown>> }
interface AssistTop { syncs?: readonly { score: number; reasons: readonly string[]; fragment: AssistFragment }[]; codes?: readonly { score: number; reasons: readonly string[]; fragment: AssistFragment }[] }

function findCurrent(feed: DecodeFeed, pipelineId: string | null, nodeId: string | null): { p: Pipeline | null; n: PipelineNode | null; block: BlockDescriptor | null } {
  const p = feed.pipelines.find((x) => x.id === pipelineId) ?? null;
  const n = p ? p.nodes.find((x) => x.id === nodeId) ?? null : null;
  const block = n ? findBlock(feed.blocks, n.block) : null;
  return { p, n, block };
}

function paramRow(schema: ParamSchema, recipeNode: RecipeNode | undefined, onApply: (name: string, value: unknown) => void): HTMLElement {
  const current = recipeNode?.params[schema.name] ?? schema.default;
  const input = h("input", { type: "text", value: current === undefined ? "" : String(current), disabled: !schema.hot });
  const err = h("small", { style: "color:var(--coral)" });
  const commit = () => {
    const r = coerceParamValue(schema, input.value);
    if (!r.ok) { err.textContent = r.error; return; }
    err.textContent = "";
    onApply(schema.name, r.value);
  };
  input.addEventListener("change", commit);
  return h("div", { class: "param" },
    h("label", {}, schema.name, schema.unit ? ` (${schema.unit})` : ""),
    h("output", {}, current === undefined ? "—" : String(current)),
    schema.hot ? input : h("span", { class: "val" }, String(current ?? "—")),
    err,
    h("div", { class: "suggest" }, h("span", {}, schema.doc || (schema.hot ? "hot: applies without a rebuild" : "requires a rebuild"))),
  );
}

export const mountParams: MountFn = (el, ctx) => {
  let feed: DecodeFeed = { pipelines: [], recipes: [], blocks: [], error: null };
  let recipeDoc: RecipeDoc | null = null;
  let recipeKey = "";
  let prevStats: { frames: number; atS: number } | null = null;
  let recordsRate: number | null = null;
  let suggestion: { nodeId: string; route: string; top: { score: number; reasons: readonly string[]; fragment: AssistFragment } | null; loading: boolean; error: string | null } | null = null;

  async function loadRecipe(p: Pipeline) {
    const key = `${p.recipe_id}@${p.recipe_version}`;
    if (key === recipeKey) return;
    recipeKey = key;
    try {
      recipeDoc = await ctx.client.get<RecipeDoc>(`/api/recipes/${encodeURIComponent(p.recipe_id)}/versions/${p.recipe_version}`);
    } catch (e) {
      ctx.store.set(toast(`could not load recipe: ${e instanceof Error ? e.message : String(e)}`));
    }
    render();
  }

  async function commitDraft(p: Pipeline, nodeId: string, draft: RecipeDoc, label: string) {
    try {
      const valid = await ctx.client.post<{ valid: boolean; errors: readonly { path: string; message: string }[] }>("/api/recipes/validate", draft);
      if (!valid.valid) { ctx.store.set(toast(`${label}: ${valid.errors[0]?.message ?? "invalid"}`)); return; }
      await ctx.client.put(`/api/pipelines/${encodeURIComponent(p.id)}/recipe`, draft);
      recipeDoc = draft;
      render();
    } catch (e) {
      ctx.store.set(toast(`${label}: ${e instanceof Error ? e.message : String(e)}`));
    }
  }

  async function runSuggest(p: Pipeline, node: PipelineNode) {
    const route = assistRouteFor(node.block);
    if (!route) return;
    suggestion = { nodeId: node.id, route, top: null, loading: true, error: null };
    render();
    try {
      const captures = await ctx.client.get<{ captures: readonly { id: string; pipeline_id: string; recording: boolean; t_last: number }[] }>("/api/captures");
      const cap = captures.captures.filter((c) => c.pipeline_id === p.id).sort((a, b) => b.t_last - a.t_last)[0];
      if (!cap) { suggestion = { ...suggestion, loading: false, error: "no recorded frames for this pipeline yet" }; render(); return; }
      const fr = await ctx.client.get<{ frames: readonly { content?: { hex?: string }; metadata?: { bit_len?: number } }[] }>(
        `/api/captures/${encodeURIComponent(cap.id)}/frames?from_t=${Math.max(0, cap.t_last - 60)}&limit=100`,
      );
      const frames = fr.frames.filter((f) => f.content?.hex).map((f) => ({ hex: f.content!.hex!, bit_len: f.metadata?.bit_len }));
      if (frames.length === 0) { suggestion = { ...suggestion, loading: false, error: "no decodable frames in the recent recording" }; render(); return; }
      const answer = await ctx.client.post<AssistTop>(route, { frames });
      const top = (answer.syncs ?? answer.codes ?? [])[0] ?? null;
      suggestion = { nodeId: node.id, route, top, loading: false, error: top ? null : "no suggestion scored above noise" };
    } catch (e) {
      suggestion = { nodeId: node.id, route, top: null, loading: false, error: e instanceof Error ? e.message : String(e) };
    }
    render();
  }

  function render() {
    const d = ctx.store.get().decode;
    const { p, n, block } = findCurrent(feed, d.pipelineId, d.nodeId);
    if (!p || !n) { el.replaceChildren(h("div", { class: "empty" }, "select a pipeline and a stage")); return; }
    void loadRecipe(p);
    // Quality comes from `pipeline.status` (the poll already carries the latest inspector status
    // tick per `hk_pipeline::recipes::runtime`), so no separate socket is needed here.
    const cur = prevStats && prevStats.frames <= p.stats.frames ? recordsPerSecond(prevStats.frames, prevStats.atS, p.stats.frames, Date.now() / 1000) : null;
    if (cur !== null) recordsRate = cur;
    prevStats = { frames: p.stats.frames, atS: Date.now() / 1000 };

    const idx = p.nodes.findIndex((x) => x.id === n.id);
    const recipeNode = recipeDoc?.nodes.find((x) => x.id === n.id);
    const route = assistRouteFor(n.block);
    const last = idx === p.nodes.length - 1;

    const paramsBlock = block && block.params.length
      ? h("div", {}, ...block.params.map((schema) => paramRow(schema, recipeNode, (name, value) => {
          if (!recipeDoc) return;
          void commitDraft(p, n.id, applyParam(recipeDoc, n.id, name, value), `${schema.name}`);
        })))
      : h("div", { class: "empty" }, "this block has no parameters");

    const suggestBlock = route
      ? h("div", { class: "param" },
          h("div", { class: "suggest" },
            suggestion?.nodeId === n.id && suggestion.loading ? h("span", {}, "searching…")
              : suggestion?.nodeId === n.id && suggestion.top
                ? h("span", {}, `Suggested: ${suggestion.top.reasons[0] ?? "match found"} (score ${suggestion.top.score.toFixed(2)})`)
                : suggestion?.nodeId === n.id && suggestion.error
                  ? h("span", {}, suggestion.error)
                  : h("span", {}, "Blind estimate available for this block"),
            h("button", { type: "button", onclick: () => void runSuggest(p, n) }, "Suggest"),
            suggestion?.nodeId === n.id && suggestion.top
              ? h("button", { type: "button", onclick: () => {
                  if (!recipeDoc || !suggestion?.top) return;
                  void commitDraft(p, n.id, applyFragment(recipeDoc, n.id, suggestion.top.fragment.params), "suggestion");
                } }, "Use")
              : null,
          ))
      : h("div", { class: "param" }, h("div", { class: "suggest" }, h("span", {}, "Blind parameter suggestions aren't served for this block yet (API gap 7a)")));

    const tiles = qualityTiles(p, n.id, recordsRate);

    el.replaceChildren(
      h("div", {},
        h("div", { class: "step" }, `${p.recipe_id} · step ${idx + 1} of ${p.nodes.length}`),
        h("h2", {}, n.id),
        h("p", {}, recipeNode?.doc || block?.doc || "no description served for this block"),
      ),
      h("div", {},
        h("div", { class: "section-h" }, "Block parameters ", h("em", {}, n.block)),
        h("div", { style: "display:flex;flex-direction:column;gap:12px" }, paramsBlock, suggestBlock),
      ),
      h("div", {},
        h("div", { class: "section-h" }, "How well it's decoding"),
        h("div", { class: "quality" }, ...tiles.map((t) => h("div", { class: "q" }, h("small", {}, t.label), h("b", { class: t.cls }, t.value)))),
      ),
      h("div", { class: "actions" },
        last
          ? h("button", { class: "act primary wide", type: "button", onclick: () => {
              ctx.client.post<{ version: number }>(`/api/pipelines/${encodeURIComponent(p.id)}/save`)
                .then((r) => ctx.store.set(toast(`${p.recipe_id}: saved as version ${r.version}`)))
                .catch((e: unknown) => ctx.store.set(toast(`save failed: ${e instanceof Error ? e.message : String(e)}`)));
            } }, h("b", {}, `Save recipe “${p.recipe_id}”`), h("small", {}, "reuse on any matching signal, no rebuild"))
          : h("button", { class: "act primary wide", type: "button", onclick: () => {
              const next = nextNodeId(p, n.id);
              if (next) ctx.store.set(selectNode(next));
            } }, h("b", {}, "Next step"), h("small", {}, p.nodes[idx + 1]?.id ?? "")),
        h("button", { class: "act", type: "button", onclick: () => {
          const out = primaryOutput(p);
          if (!out) { ctx.store.set(toast("this pipeline has no output to stream")); return; }
          startRecordsOutput(ctx, { pipelineId: p.id, outputId: out.id, label: `${p.recipe_id} · ${out.id}` });
        } }, h("b", {}, "Stream records"), h("small", {}, "to Outputs / TCP")),
        h("button", { class: "act", type: "button", onclick: () => { recipeKey = ""; void loadRecipe(p); } }, h("b", {}, "Reset params"), h("small", {}, "to the saved recipe")),
      ),
    );
  }

  subscribeDecodeFeed(ctx, (f) => { feed = f; render(); });
  ctx.store.select((s) => s.decode, () => { suggestion = null; render(); });
  render();
};
