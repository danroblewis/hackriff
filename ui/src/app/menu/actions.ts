// Builds the context menu's items for a focused signal or selection (T-192, docs/14-ui-rewrite.md
// "Added scope from docs/15 §7"). Every item calls exactly the API call the focus-bar button used
// to call (ADR-0013 §4.5) via the existing helpers in explore/focus.ts, explore/inventory.ts,
// explore/selections.ts and dock/api.ts — this module adds no new signal logic, only menu wiring
// plus the new Analyze stub call (T-190 lands the backend later).
import { ControlError } from "../../controls/client";
import type { AppContext } from "../context";
import { startListen, stopOutput } from "../dock/api";
import { apiErrorText } from "../explore/format";
import { decodeActionLabel, emitterStreamAddress, recordEmitterClip } from "../explore/focus";
import { deleteEntry, loadInventoryRows, promoteEntry, type Row } from "../explore/inventory";
import { listenAllTargets, recordSelectionClip, selectionStoreFor, type Selection } from "../explore/selections";
import { focusSignal, removeInventoryRowLocal, restoreInventoryRowLocal } from "../explore/slice";
import { setMode, toast } from "../state";
import type { MenuItem } from "./model";

const fmtMHz = (hz: number) => (hz / 1e6).toFixed(4);

/** The dock entry id for `emitterId`'s live/opening audio Listen, or null when it isn't playing
 * (mirrors explore/index.ts's `isOn`; small enough to duplicate rather than export across an
 * otherwise one-directional import: explore/index.ts imports this module, not the reverse). */
function listenIdFor(ctx: AppContext, emitterId: string): string | null {
  const e = ctx.store.get().outputs.find((o) => o.kind === "audio" && o.emitterId === emitterId && (o.state === "live" || o.state === "opening"));
  return e ? e.id : null;
}

export type AnalyzeTarget = { kind: "emitter" | "selection"; id: string };
export type AnalyzeResult = { ok: true; message: string } | { ok: false; notImplemented: boolean; message: string };

/** The client surface [[analyzeTarget]] needs (a test double only has to implement `post`, like
 * `explore/selections.ts`'s `PostClient`). */
export interface PostClient { post<T>(path: string, body?: unknown): Promise<T> }

/** "Analyze / synthesize decoder": `POST /api/analyze` with `{emitter_id}` or `{selection_id}`.
 * Until T-190 lands the backend this answers 501 `not_implemented` (or the route itself answers
 * 404 on an older server); both report as a non-error "not implemented yet" notice rather than an
 * error, same shape as `recordEmitterClip`/`recordSelectionClip` so the menu item just toasts the
 * message either way. */
export async function analyzeTarget(client: PostClient, target: AnalyzeTarget): Promise<AnalyzeResult> {
  const body = target.kind === "emitter" ? { emitter_id: target.id } : { selection_id: target.id };
  try {
    await client.post("/api/analyze", body);
    return { ok: true, message: "Analyze: requested" };
  } catch (e) {
    if (e instanceof ControlError && (e.status === 404 || (e.status === 501 && e.code === "not_implemented"))) {
      return { ok: false, notImplemented: true, message: "Analyze: not implemented yet" };
    }
    return { ok: false, notImplemented: false, message: `Analyze: ${apiErrorText(e)}` };
  }
}

/** Menu items for a right-clicked/long-pressed signal (an inventory row or a waterfall bracket):
 * the same action set the old focus-bar buttons carried (§4.5), plus Analyze. */
export function signalMenuItems(ctx: AppContext, r: Row): MenuItem[] {
  const onId = listenIdFor(ctx, r.id);
  const reload = () => loadInventoryRows(ctx, () => {});

  const items: MenuItem[] = [
    {
      id: "listen", label: onId ? "Stop listening" : "Listen", hint: onId ? "removes from Outputs" : "adds to Outputs",
      onSelect: () => { if (onId) stopOutput(ctx, onId); else startListen(ctx, { kind: "emitter", emitterId: r.id, label: `${fmtMHz(r.f_center_hz)} MHz` }); },
    },
    {
      id: "decode", label: decodeActionLabel(r), hint: "build a pipeline",
      onSelect: () => ctx.store.set(setMode("decode")),
    },
    {
      id: "analyze", label: "Analyze", hint: "synthesize decoder",
      onSelect: () => { void analyzeTarget(ctx.client, { kind: "emitter", id: r.id }).then((res) => ctx.store.set(toast(res.message))); },
    },
    {
      id: "export", label: "Export clip", hint: "from the buffer",
      onSelect: () => {
        void recordEmitterClip(ctx.client, r.id).then((res) => ctx.store.set(toast(res.ok ? `recording ${res.kinds.join(", ")}` : `export: ${res.message}`)));
      },
    },
    {
      id: "stream", label: "Stream out", hint: "audio",
      onSelect: () => {
        void emitterStreamAddress(ctx.client, r.id).then((addr) => {
          if (!addr) { ctx.store.set(toast("stream out: not offered by this server")); return; }
          navigator.clipboard?.writeText(addr).catch(() => {});
          ctx.store.set(toast(`copied ${addr}`));
        });
      },
    },
  ];

  if (r.state === "candidate") {
    items.push({
      id: "promote", label: "Promote", hint: "to confirmed",
      onSelect: () => { void promoteEntry(ctx.client, r.id, reload).then((res) => { if (!res.ok) ctx.store.set(toast(`promote ${r.id}: ${res.message}`)); }); },
    });
  }
  items.push({
    id: "delete", label: r.state === "candidate" ? "Delete" : "Delete from inventory",
    hint: r.state === "candidate" ? "detections kept" : "detections and history kept", danger: true,
    onSelect: () => {
      // T-187: optimistic — remove the row before the server confirms; put it back on a refusal.
      ctx.store.set(removeInventoryRowLocal(r.id));
      void deleteEntry(ctx.client, r.id, reload).then((res) => {
        if (!res.ok) {
          ctx.store.set(restoreInventoryRowLocal(r));
          ctx.store.set(toast(`delete ${r.id}: ${res.message}`));
        }
      });
    },
  });
  items.push({
    id: "adjust-band", label: "Adjust band", hint: "draggable edges land in T-193",
    onSelect: () => {
      ctx.store.set(focusSignal(r.id));
      ctx.store.set(toast("Adjust band: focused — drag the box edges once T-193 lands."));
    },
  });
  return items;
}

/** Menu items for a right-clicked/long-pressed selection: the actions the selection's focus panel
 * carried (§4.5 — Listen to all, Export clip, Delete), plus Analyze. No Decode/Stream
 * out/Promote/Adjust band: those never applied to a selection. */
export function selectionMenuItems(ctx: AppContext, s: Selection, rowsInside: readonly Row[]): MenuItem[] {
  return [
    {
      id: "listen-all", label: "Listen to all", hint: `${rowsInside.length} stream${rowsInside.length === 1 ? "" : "s"} at once`,
      disabled: rowsInside.length === 0,
      onSelect: () => { for (const t of listenAllTargets(rowsInside)) startListen(ctx, t); },
    },
    {
      id: "analyze", label: "Analyze", hint: "synthesize decoder",
      onSelect: () => { void analyzeTarget(ctx.client, { kind: "selection", id: s.id }).then((res) => ctx.store.set(toast(res.message))); },
    },
    {
      id: "export", label: "Export clip", hint: "from the buffer",
      onSelect: () => {
        void recordSelectionClip(ctx.client, s).then((res) => ctx.store.set(toast(res.ok ? `recording ${res.kinds.join(", ")}` : `export: ${res.message}`)));
      },
    },
    {
      id: "delete", label: "Delete selection", hint: "detections kept", danger: true,
      onSelect: () => selectionStoreFor(ctx).remove(s.id),
    },
  ];
}
