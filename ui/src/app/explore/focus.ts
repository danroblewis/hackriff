// Focus-panel model (ADR-0013 §4.5, T-151): pure view-model helpers for the focused signal or
// selection, plus the actions that read fields already served by the API — nothing here estimates
// a signal parameter itself.
import { apiErrorText } from "./format";
import { RECORD_KINDS } from "../../outputs";
import type { Row } from "./inventory";
import type { Selection } from "./selections";
import type { InventoryWindow, WindowCoverage } from "./slice";

export interface ApiClient { get<T>(path: string): Promise<T>; post<T>(path: string, body?: unknown): Promise<T> }

/** The Decode action's label: names the known identity scheme when there is one (RDS, …), else the
 * generic invitation — never claims a decode will run automatically (GAP 7b: recipes are listed,
 * never auto-started). */
export function decodeActionLabel(r: Pick<Row, "identity_scheme">): string {
  const scheme = r.identity_scheme?.split("-")[0];
  return scheme ? `Decode ${scheme.toUpperCase()}` : "Open in Decode";
}

/** Starts a recording of this emitter forward from now (§4.5 "Record / Export clip"; GAP 1: not
 * from the always-on buffer yet). */
export async function recordEmitterClip(client: ApiClient, emitterId: string): Promise<{ ok: true; kinds: string[] } | { ok: false; message: string }> {
  try {
    const r = await client.post<{ recording: { kinds: string[] } }>("/api/outputs/record/start", {
      emitter_id: emitterId, kinds: RECORD_KINDS,
    });
    return { ok: true, kinds: r.recording.kinds };
  } catch (e) {
    return { ok: false, message: apiErrorText(e) };
  }
}

interface StreamsInfo { tcp: { addr: string } | null; on_demand: { name: string; tcp_target: string }[] }

/** "Stream out" address for an emitter's audio (§4.5; IQ stream-out is GAP 8, not offered here):
 * `tcp://<addr> <tcp_target>?emitter=<id>`, the same shape the dock's Copy address uses, without
 * the token. `null` when no TCP stream server or `listen` opener is offered. */
export async function emitterStreamAddress(client: ApiClient, emitterId: string): Promise<string | null> {
  const r = await client.get<StreamsInfo>("/api/streams");
  const listen = r.on_demand.find((o) => o.name === "listen");
  if (!r.tcp || !listen) return null;
  return `tcp://${r.tcp.addr} ${listen.tcp_target}?emitter=${encodeURIComponent(emitterId)}`;
}

// ---- a focused signal that is not among the window's rows (T-385) ----

/**
 * What `GET /api/inventory/{id}` says about an emitter's existence, as this panel reads it.
 *
 * `{ state }` is the entry's own lifecycle state — the route serves **deleted entries too**
 * (docs/api.md `/api/inventory/{id}`), which is precisely what makes a real deletion recognisable.
 * `"gone"` is a `404`: no such entry at all. `"failed"` is an answer that never came, and it stays
 * *unknown* rather than hardening into either claim (T-379's rule for a missing coverage answer,
 * which is the same rule).
 */
export type EmitterLookup = { state: string } | "gone" | "failed";

/** A loaded value, `"loading"` while its fetch is in flight, or `undefined` before it starts. */
export type Loaded<T> = T | "loading" | undefined;

/**
 * What the focus panel shows for `focus.kind === "signal"` — and the point of the type is that
 * **`"deleted"` and `"out-of-window"` are different claims about different things** (T-385).
 *
 * The inventory is time-scoped to the view (CLAUDE.md invariant 2), so a focused row leaving
 * `inventory.rows` is the *expected* case: the window moved, or the frequency view did. Rendering
 * that as "no longer in the inventory" asserts a deletion the UI never observed — the whole-UI
 * window rule's "a surface may render empty only where data genuinely does not exist", applied to a
 * panel instead of to a list. The mirrored bug is just as bad: an emitter the user really did
 * delete must still say so, not hide behind a reassuring "outside the window".
 */
export type SignalFocus =
  | { kind: "row"; row: Row }
  | { kind: "deleted" }
  | { kind: "gone" }
  | { kind: "out-of-window"; coverage: WindowCoverage }
  | { kind: "no-window" }
  | { kind: "checking" }
  | { kind: "unchecked" };

/**
 * Resolves the focused signal against the window the lists were asked about and the emitter lookup.
 *
 * Order matters: an emitter's *existence* is window-independent, so a decisive lookup outranks the
 * window — a deletion is still a deletion while the capture window is unknown. Everything else is a
 * statement about this window, and nothing is concluded from the row's absence alone.
 */
export function signalFocus(
  row: Row | undefined, window: InventoryWindow | null, lookup: Loaded<EmitterLookup>,
): SignalFocus {
  if (row) return { kind: "row", row };
  if (lookup === "gone") return { kind: "gone" };
  if (lookup !== undefined && lookup !== "loading" && lookup !== "failed" && lookup.state === "deleted") return { kind: "deleted" };
  if (window === null) return { kind: "no-window" };
  if (lookup === undefined || lookup === "loading") return { kind: "checking" };
  if (lookup === "failed") return { kind: "unchecked" };
  return { kind: "out-of-window", coverage: window.coverage };
}

/**
 * The sentence for a focused signal that is not on screen, extending T-379's four-way emptiness to
 * this panel (docs/14 "The whole UI is one window"): the same vocabulary, never a parallel one.
 *
 * | state | what it means |
 * |---|---|
 * | `deleted` | the entry exists and its lifecycle state is `deleted` — a real deletion, said plainly |
 * | `gone` | `404`: there is no such entry at all |
 * | `out-of-window` | the entry exists and is not deleted; it is simply not in the window on screen |
 * | `no-window` | the UI does not know which window to ask about (T-379's first emptiness, verbatim) |
 * | `checking` | the lookup is in flight; nothing is claimed yet |
 * | `unchecked` | the lookup never answered — unknown, not deleted |
 *
 * Under `out-of-window` the window's own coverage still qualifies the claim: over a window nothing
 * ever sampled, this row's absence is not evidence about the row.
 */
export function signalFocusText(f: Exclude<SignalFocus, { kind: "row" }>): string {
  switch (f.kind) {
    case "deleted": return "That signal was deleted from the inventory.";
    case "gone": return "That signal is no longer in the inventory.";
    case "no-window": return "Waiting for the capture window…";
    case "checking": return "Checking the inventory…";
    case "unchecked": return "Could not check whether that signal is still listed.";
    case "out-of-window":
      if (f.coverage === "unobserved") return "Not in this window — nothing was observed here, so it is unobserved, not gone.";
      if (f.coverage === "observed") return "Not in the window on screen — outside the time or frequency range you are viewing, not gone.";
      return "Not listed for this window.";
  }
}

/**
 * Asks whether the emitter still exists, and in what lifecycle state (`GET /api/inventory/{id}`).
 *
 * This is the **only** thing that can tell a deleted emitter from one that is merely outside the
 * window on screen; the row's absence from the list says nothing on its own, because the list is
 * window-scoped by design. A `404` is `"gone"`; any other failure is `"failed"` — unknown — and
 * never an invented deletion.
 */
export async function fetchEmitterLookup(client: ApiClient, emitterId: string): Promise<EmitterLookup> {
  try {
    const row = await client.get<{ state?: unknown }>(`/api/inventory/${encodeURIComponent(emitterId)}`);
    return typeof row?.state === "string" ? { state: row.state } : "failed";
  } catch (e) {
    const err = e as { status?: unknown; code?: unknown };
    return err?.status === 404 || err?.code === "not_found" ? "gone" : "failed";
  }
}

/** Selection panel header text: extent width plus how many emitters are inside. */
export function selectionSummary(s: Selection, insideCount: number): string {
  const wideHz = s.f_hi - s.f_lo;
  const wide = wideHz >= 1e6 ? `${(wideHz / 1e6).toFixed(2)} MHz` : `${Math.round(wideHz / 1e3)} kHz`;
  return `${wide} wide · ${insideCount} signal${insideCount === 1 ? "" : "s"} inside`;
}
