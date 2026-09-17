// Packet inspector (ADR-0013 §2 slot "inspector", §3.1 `inspector` slice, §4.7, §8 T-154 brief).
// Rehomes the M1 inspector (T-090, `ui/src/frame-inspector.ts`) into Decode mode: a frame list, a
// hex+ASCII view and a layer tree, with linked selection both ways. Frames arrive live over
// `decode/status-feed.ts`'s `subscribePipelineFeed` (T-153's one-socket-per-pipeline contract).
//
// Thin client: this file does no parsing. Every byte range, field value, type and fit/error comes
// from the API's layer tree (stream-contract §14.2); the pure helpers below only reuse
// `frame-inspector.ts`'s tree/byte-index logic (`buildTree`, `cycleLeafAt`, `nodeById`, `hexBytes`,
// `asciiChar`) plus view-model bookkeeping that is presentation only: ring-buffer paging, hex-row
// chunking and a byte's display colour from its own already-known `type`/`error` fields.
import { h } from "../dom";
import type { AppContext, MountFn } from "../context";
import {
  asciiChar, cycleLeafAt, errorsByPath, frameViewFromCapture, hexBytes, nodeById,
  type FrameRecordDto, type FrameView, type LayerError, type LayerNode, type LayerTree,
} from "../../frame-inspector";
import {
  viewWindow, windowCoverage, windowEmptyText, windowKey,
  type ViewWindow, type WindowEmptiness, type WindowState,
} from "../explore/inventory";
import { captureFor, captureFramesPath, type CaptureLite } from "./captures";
import { selectInspectorField, selectInspectorFrame, type InspectorSlice } from "./inspector-slice";
import { subscribePipelineFeed, type FeedState, type FrameRecord } from "./status-feed";

export const RING_CAP = 200;

/** One ring-buffered frame, plus a one-shot "just arrived" flag for the frame list's flash. */
export interface RingFrame { view: FrameView; fresh: boolean }

/** A live inspector-stream frame record (stream-contract §14.2) carries the same fields as a
 * capture page's frame record (`frame-inspector.ts`'s `FrameRecordDto`), so it reuses
 * `frameViewFromCapture` as-is rather than parsing the record again. */
export function frameViewFromLive(f: FrameRecord, fallbackIndex: number): FrameView {
  return frameViewFromCapture(f as unknown as FrameRecordDto, fallbackIndex);
}

/** Prepends a new frame (newest first), resetting older frames' `fresh` flag so a full re-render
 * of the list only flashes the frame that actually just arrived, capped at `cap`. */
export function pushRingFrame(ring: readonly RingFrame[], view: FrameView, cap = RING_CAP): RingFrame[] {
  return [{ view, fresh: true }, ...ring.map((r) => (r.fresh ? { ...r, fresh: false } : r))].slice(0, cap);
}

/** The frame the panes should show: the selected sequence if it's still in the ring, else the
 * newest frame (or none). */
export function resolveSelectedFrame(ring: readonly RingFrame[], frameSeq: number | null): FrameView | null {
  if (frameSeq !== null) {
    const found = ring.find((r) => r.view.index === frameSeq);
    if (found) return found.view;
  }
  return ring[0]?.view ?? null;
}

// ---- the window this panel is a view over (T-387) --------------------------------------------
//
// **The prior question, answered.** T-384 left four surfaces on the live edge and said why: the
// packet inspector, status feed, pipelines list and outputs dock are live WebSocket transports, and
// `/ws/open/inspector`/`/ws/open/stage` have no history form. T-387 asked whether each *should*
// re-derive at all. Three describe the run rather than the air and are honestly live-only, and say
// so (`app/live-only.ts`). This one is different: **packets are data about the air.** Every frame
// record carries its own capture-clock `t_ns` (stream contract §5.1), and the frames of a past
// window exist — the pipeline's inspector output is recorded to a capture with no request (§14.7).
// So the whole-UI window rule bites here in full: the data exists for the window, therefore it must
// be shown.
//
// **And it needed no contract change.** `GET /api/captures/{id}/frames?from_t=&to_t=` already
// carries the window and already serves these exact records; `docs/api.md` names it "the right
// route for the packet inspector's own scrubbing". A route that exists beats growing a history
// form on an on-demand opener (ADR-0004), which is what T-384 correctly refused to do in passing.
//
// The live socket stays. It is bounded by **count**, never by a clock (T-384's `capFrames` rule):
// a time-trimmed ring would discard live frames while the view is scrubbed back, so returning to
// Live would find the list missing frames it had already received.

/** A frame's key for de-duplication across the two sources: its own capture-clock time and the
 * pipeline's own frame number, both of which a live record and its recorded copy share. */
function frameKey(v: FrameView): string {
  return `${v.timeS}/${v.index}`;
}

/**
 * Whether a frame falls in the window, on the **capture clock**.
 *
 * `timeS` is `t_ns / 1e9` — the frame's own stamp, never the browser's. This is the bug T-379 found
 * in the Candidate list, T-384 found at three sites in `plots.ts` and T-389 found in the live
 * Confirmed query; on the fixture behind T-379 a browser instant sits 3.5 days from the capture
 * clock, so a window built from `Date.now()` selects either nothing or everything and both read on
 * screen as an answer.
 *
 * A frame with no `t_ns` is **not** in the window: it cannot be placed on the time axis at all, and
 * claiming it for the window on screen would be asserting a time nobody measured. They are counted
 * and disclosed instead of silently dropped — see [[unplaceableFrames]].
 */
export function frameInWindow(v: FrameView, w: ViewWindow): boolean {
  return v.timeS !== undefined && v.timeS >= w.t0 && v.timeS <= w.t1;
}

/** Live frames carrying no `t_ns`, so no window can contain them. Reported in the panel's note
 * rather than dropped in silence: an unplaceable frame is a gap in the record, not a non-event. */
export function unplaceableFrames(ring: readonly RingFrame[]): number {
  return ring.reduce((n, r) => n + (r.view.timeS === undefined ? 1 : 0), 0);
}

/**
 * The frames of the view window: what the live socket received, plus what the pipeline's capture
 * holds for a stretch the socket never saw, de-duplicated and newest-first.
 *
 * Both sources are filtered by the **same** predicate even though the route was already asked for
 * `[t0, t1]` — `to_t` ends a page at the first frame *after* it, so the route may serve one frame
 * past the window, and one predicate in one place is what keeps the list and the waterfall above it
 * from disagreeing about which frames are inside.
 *
 * A window straddling the live edge is served by both sources, so a frame would otherwise be listed
 * twice; [[frameKey]] drops the recorded copy and keeps the live one, which is the one carrying the
 * `fresh` flash.
 */
export function windowFrames(
  ring: readonly RingFrame[], stored: readonly FrameView[], w: ViewWindow,
): RingFrame[] {
  const live = ring.filter((r) => frameInWindow(r.view, w));
  const seen = new Set(live.map((r) => frameKey(r.view)));
  const recorded = stored
    .filter((v) => frameInWindow(v, w) && !seen.has(frameKey(v)))
    .map((view) => ({ view, fresh: false }));
  return [...live, ...recorded].sort(
    (a, b) => (b.view.timeS ?? 0) - (a.view.timeS ?? 0) || b.view.index - a.view.index,
  );
}

/** What the frame list has for the window it asked about. */
export type FrameListView =
  | { kind: "frames"; frames: RingFrame[] }
  | Exclude<WindowEmptiness, { kind: "error" }>;

/**
 * What an empty frame list says, in the vocabulary every window-scoped surface shares
 * ([[windowEmptyText]]): *no window known* and *nothing was ever observed here* are claims about
 * the measurement and are worded identically everywhere; only "No frames in this window." is a
 * finding about the air, and only it may be said when the receiver really was sampling.
 */
export function frameListEmptyText(v: Exclude<FrameListView, { kind: "frames" }>): string {
  return windowEmptyText(v, "No frames in this window.", "No frames listed for this window.");
}

/** The note above the frame list: what the window holds, then what the live tap is doing, then
 * where the stream is served. The three are separated because they are three different facts — a
 * connected tap says nothing about whether the *window on screen* holds frames. */
export function inspectorNoteText(view: FrameListView, unplaceable: number, feed: string, served: string): string {
  const windowPart = view.kind === "frames"
    ? `${view.frames.length} frame${view.frames.length === 1 ? "" : "s"} in this window`
    : frameListEmptyText(view);
  const parts = [windowPart];
  if (unplaceable > 0) parts.push(`${unplaceable} carry no time and cannot be placed`);
  if (feed) parts.push(`${feed} tap`);
  parts.push(served);
  return parts.join(" · ");
}

/** The `GET /api/captures` + `/frames` client this panel needs; `AppContext["client"]` satisfies
 * it structurally. */
export interface FramesClient { get<T>(path: string): Promise<T> }

/** The window's recorded frames, and the window they are of — so a stale set is never rendered
 * under a new window. */
export interface FrameBackfill { w: ViewWindow; frames: FrameView[] }

/**
 * Fetches the window's frames from the pipeline's own capture.
 *
 * **This never re-decodes.** `/api/captures/{id}/frames` serves records the pipeline already wrote,
 * which is what CLAUDE.md's incremental-decode invariant asks for: live decoding extends a region
 * and decodes only the newly-arrived part, so answering a scrub must be a read.
 *
 * Best-effort and additive: a failure, no capture store, or a pipeline with nothing recorded yields
 * an empty set *for that window* rather than emptying the live frames — the window is still the
 * window, and the live socket's own frames for it are still shown.
 */
export async function loadFrameBackfill(
  client: FramesClient, pipelineId: string, w: ViewWindow,
): Promise<FrameBackfill> {
  try {
    const list = await client.get<{ captures?: readonly CaptureLite[] }>("/api/captures");
    const cap = captureFor(list.captures ?? [], pipelineId);
    if (!cap) return { w, frames: [] };
    const page = await client.get<{ frames?: readonly FrameRecordDto[] }>(captureFramesPath(cap.id, w));
    return { w, frames: (page.frames ?? []).map((f, i) => frameViewFromCapture(f, i)) };
  } catch {
    return { w, frames: [] };
  }
}

/**
 * The frame list for the current window, and — when it is empty — which emptiness it is.
 *
 * Coverage is asked for only when the window came back with nothing, the one case where the
 * difference between "this band was quiet" and "nothing ever looked here" is the whole message. An
 * answer that never comes stays *unknown* ([[windowCoverage]] returns `null`), so a missing answer
 * never hardens into a measurement claim.
 */
export async function resolveFrameList(
  client: FramesClient, state: WindowState, ring: readonly RingFrame[], backfill: FrameBackfill | null,
): Promise<FrameListView> {
  const w = viewWindow(state);
  // No window: render nothing and say so. An invented window selects an honest zero frames, and
  // zero frames reads on screen exactly like a decoder that produced none (T-379 obligation 4).
  if (w === null) return { kind: "no-window" };
  const stored = backfill && backfill.w.t0 === w.t0 && backfill.w.t1 === w.t1 ? backfill.frames : [];
  const frames = windowFrames(ring, stored, w);
  if (frames.length > 0) return { kind: "frames", frames };
  return { kind: "empty", coverage: await windowCoverage(client, state, w) };
}

// ---- frame list view model ----

export interface FrameRowVM {
  seq: number;
  timeText: string;
  channelText: string;
  summaryText: string;
  bad: boolean;
  gated: boolean;
  fresh: boolean;
  selected: boolean;
}

function fmtTimeS(s: number | undefined): string {
  return s === undefined || !Number.isFinite(s) ? "—" : s.toFixed(2);
}

/** Builds the frame list's rows from the ring, resolving which one is selected the same way
 * [[resolveSelectedFrame]] does (an explicit selection if still present, else the newest). */
export function frameRowsVM(ring: readonly RingFrame[], frameSeq: number | null): FrameRowVM[] {
  const selected = resolveSelectedFrame(ring, frameSeq);
  return ring.map((r) => {
    const v = r.view;
    return {
      seq: v.index,
      timeText: fmtTimeS(v.timeS),
      channelText: v.channel !== undefined ? String(v.channel) : v.channelHz !== undefined ? `${(v.channelHz / 1e6).toFixed(4)} MHz` : "—",
      summaryText: v.gated ? "withheld" : v.crcStatus ? v.crcStatus : v.fit,
      bad: !v.gated && (v.crcStatus === "invalid" || v.fit === "failed"),
      gated: !!v.gated,
      fresh: r.fresh,
      selected: selected === v,
    };
  });
}

// ---- served address (docs/api.md "GET /api/streams" — discovery) ----

export interface StreamInfo { stream_id: string; tcp_target: string }
export interface StreamsResponse { streams?: StreamInfo[]; tcp?: { addr: string } | null }

/** The pipeline's first inspector-output stream entry (`inspector/<pipeline>/<output>`). */
export function findInspectorStream(streams: StreamsResponse, pipelineId: string): StreamInfo | null {
  return (streams.streams ?? []).find((s) => s.stream_id.startsWith(`inspector/${pipelineId}/`)) ?? null;
}

/** "tcp://<addr> <target>" for Decode's served-address note. Never includes the token
 * (docs/api.md §4.8 "Copy address": the handshake token goes on the wire, not in the copied text). */
export function servedAddressText(streams: StreamsResponse, pipelineId: string): string {
  const s = findInspectorStream(streams, pipelineId);
  if (!s) return "not streaming yet";
  if (!streams.tcp?.addr) return `${s.stream_id} (no TCP bridge on this server)`;
  return `tcp://${streams.tcp.addr} ${s.tcp_target}`;
}

// ---- byte colouring by owning leaf field ----
// Presentation only: a byte's colour comes from its owning leaf's own `type`/`error` fields
// (stream-contract §14.2), already served by the API. No range arithmetic beyond reading
// `byte_index`/`bytes`, which the M1 inspector's `cycleLeafAt`/`nodeById` already provide.

const TYPE_CLASS: Readonly<Record<string, string>> = {
  uint: "b-teal", int: "b-teal", enum: "b-amber", ascii: "b-cream",
  bitfield: "b-lav", flag: "b-lav", bytes: "b-mut", layer: "b-mut",
};

/** The leaf field owning byte `b` (stream-contract §14.2 `byte_index[b][0]`), or null outside any field. */
export function byteOwner(tree: LayerTree, byte: number): LayerNode | null {
  const ids = tree.byte_index[byte];
  return ids && ids.length ? nodeById(tree, ids[0]) : null;
}

/** CSS class for one byte: its owning leaf's type, or `b-coral` when the leaf carries a fit error. */
export function byteColorClass(tree: LayerTree, byte: number): string {
  const n = byteOwner(tree, byte);
  if (!n) return "b-mut";
  return n.error ? "b-coral" : (TYPE_CLASS[n.type] ?? "b-mut");
}

// ---- hex + ASCII rows ----

export interface HexCell { byte: number; hex: string; ascii: string; colorClass: string; selected: boolean }
export interface HexRow { offset: string; cells: HexCell[] }

/** Chunks a frame's bytes into 16-wide hex/ASCII rows, colouring and highlighting each byte from
 * the layer tree and the current selection range (inclusive-exclusive, as `LayerNode.bytes`). */
export function hexRows(bytes: readonly number[], tree: LayerTree | undefined, selectedRange: readonly [number, number] | null, perRow = 16): HexRow[] {
  const rows: HexRow[] = [];
  for (let r = 0; r < bytes.length; r += perRow) {
    const cells: HexCell[] = [];
    for (let i = r; i < Math.min(r + perRow, bytes.length); i++) {
      cells.push({
        byte: i,
        hex: bytes[i].toString(16).toUpperCase().padStart(2, "0"),
        ascii: asciiChar(bytes[i]),
        colorClass: tree ? byteColorClass(tree, i) : "b-mut",
        selected: !!selectedRange && i >= selectedRange[0] && i < selectedRange[1],
      });
    }
    rows.push({ offset: r.toString(16).padStart(4, "0"), cells });
  }
  return rows;
}

// ---- linked selection (stream-contract §14.2 "Linked selection uses only these values") ----

/** Field → bytes: the field's own `bytes` range, or null (no field selected, or not in this tree). */
export function selectedByteRange(tree: LayerTree | undefined, fieldNodeId: number | null): [number, number] | null {
  if (!tree || fieldNodeId === null) return null;
  const n = nodeById(tree, fieldNodeId);
  return n ? n.bytes : null;
}

// ---- DOM mount ----

const COLORS: Readonly<Record<string, string>> = { uint: "b-teal", int: "b-teal", enum: "b-amber", ascii: "b-cream", bitfield: "b-lav", flag: "b-lav", bytes: "b-mut", layer: "b-mut" };
const nodeColorClass = (n: LayerNode) => (n.error ? "b-coral" : (COLORS[n.type] ?? "b-mut"));

// ---- pipeline/selection sources (T-195): the Decode workbench binds to its own pipeline picker
// and shares one global frame/field selection (`state.inspector`), unchanged from T-154. A
// per-signal output panel (`explore/output-panel.ts`) instead binds to one fixed pipeline and gets
// its own independent selection, so two panels never fight over which frame is selected.

export interface PipelineSource { get(): string | null; subscribe(cb: (id: string | null) => void): () => void }

/** The Decode workbench's own pipeline selection (`decode.pipelineId`). */
export function decodeSelectionPipeline(ctx: AppContext): PipelineSource {
  return { get: () => ctx.store.get().decode.pipelineId, subscribe: (cb) => ctx.store.select((s) => s.decode.pipelineId, cb) };
}

/** A pipeline id fixed for the panel's whole lifetime (a per-signal output panel bound to one
 * emitter's currently-running decode pipeline; T-195 never retargets a mounted panel). */
export function fixedPipeline(id: string | null): PipelineSource {
  return { get: () => id, subscribe: () => () => {} };
}

export interface SelectionPort {
  get(): InspectorSlice;
  setFrame(frameSeq: number | null): void;
  setField(fieldNodeId: number | null): void;
  /** Fires on any change to either field. */
  subscribe(cb: () => void): () => void;
}

/** The Decode workbench's shared selection (`state.inspector`), as before T-195. */
export function storeSelection(ctx: AppContext): SelectionPort {
  return {
    get: () => ctx.store.get().inspector,
    setFrame: (frameSeq) => ctx.store.set(selectInspectorFrame(frameSeq)),
    setField: (fieldNodeId) => ctx.store.set(selectInspectorField(fieldNodeId)),
    subscribe: (cb) => ctx.store.select((s) => s.inspector, () => cb()),
  };
}

/** An independent selection for one per-signal output panel (T-195): several panels, or a panel
 * and the Decode workbench, never share a frame/field selection. */
export function localSelection(): SelectionPort {
  let sel: InspectorSlice = { frameSeq: null, fieldNodeId: null };
  const listeners = new Set<() => void>();
  return {
    get: () => sel,
    setFrame(frameSeq) { sel = { frameSeq, fieldNodeId: null }; for (const l of [...listeners]) l(); },
    setField(fieldNodeId) { sel = { ...sel, fieldNodeId }; for (const l of [...listeners]) l(); },
    subscribe(cb) { listeners.add(cb); return () => listeners.delete(cb); },
  };
}

class InspectorPanel {
  private ring: RingFrame[] = [];
  /** The window's recorded frames, for a stretch the live socket never carried (T-387). */
  private backfill: FrameBackfill | null = null;
  /** What the current window holds — the list, the panes and the note all read this, so they can
   * never disagree about which frames are in the window. */
  private view: FrameListView = { kind: "no-window" };
  private unsubscribe: () => void = () => {};
  private unsubWindow: () => void = () => {};
  private pipelineId: string | null = null;
  private loadSeq = 0;
  private viewSeq = 0;
  private backfillSeq = 0;
  private refreshTimer: ReturnType<typeof setTimeout> | null = null;
  private feedState: FeedState = "closed";
  private feedMessage = "";
  private servedText = "select a pipeline in Decode";
  private byteCycle: { byte: number; id: number } | null = null;
  private nodeRows = new Map<number, HTMLElement>();
  private disposers: (() => void)[] = [];

  private frameListEl: HTMLElement;
  private noteEl: HTMLElement;
  private byteNoteEl: HTMLElement;
  private hexEl: HTMLElement;
  private flowEl: HTMLElement;
  private treeEl: HTMLElement;

  constructor(
    el: HTMLElement,
    private ctx: AppContext,
    private pipelineSource: PipelineSource = decodeSelectionPipeline(ctx),
    private selection: SelectionPort = storeSelection(ctx),
  ) {
    this.frameListEl = h("div", { class: "insp-frame-list", role: "list" });
    this.noteEl = h("em", {}, this.servedText);
    this.byteNoteEl = h("em", {}, "hex · ASCII · click a byte or a field");
    this.hexEl = h("div", { class: "insp-hexgrid" });
    this.flowEl = h("div", { class: "insp-flow" });
    this.treeEl = h("div", { class: "insp-tree", "aria-label": "Packet fields" });
    el.replaceChildren(
      h("div", { class: "insp" },
        h("div", { class: "insp-frames" },
          h("div", { class: "insp-head" }, h("div", { class: "section-h" }, h("span", {}, "Output stream"), this.noteEl)),
          this.frameListEl),
        h("div", { class: "insp-bytes" },
          h("div", { class: "section-h" }, h("span", {}, "Frame bytes"), this.byteNoteEl),
          this.hexEl, this.flowEl),
        this.treeEl),
    );
    this.disposers.push(this.pipelineSource.subscribe((id) => this.onPipeline(id)));
    this.onPipeline(this.pipelineSource.get());
    this.disposers.push(this.selection.subscribe(() => this.renderFrameList()));
    // Scrubbing must re-derive the list, not leave the live edge's packets on screen under a past
    // window's heading (T-384's lesson on the RDS panel, applied here). `windowKey` changes exactly
    // when the window does — including when the live edge advances — so a panel watching only the
    // cursor could not go on answering about the window it was mounted in.
    this.disposers.push(this.ctx.store.select(windowKey, () => this.onWindowChanged()));
  }

  /** Closes the pipeline feed subscription and store listeners. Call when a per-signal output
   * panel is torn down (e.g. its tab is no longer shown); the Decode workbench's singleton never
   * calls this since it lives for the page's lifetime. */
  destroy(): void {
    this.unsubscribe();
    this.unsubWindow();
    if (this.refreshTimer !== null) { clearTimeout(this.refreshTimer); this.refreshTimer = null; }
    for (const d of this.disposers) d();
  }

  private onPipeline(id: string | null) {
    this.unsubscribe();
    this.unsubWindow();
    this.unsubWindow = () => {};
    this.pipelineId = id;
    this.ring = [];
    this.backfill = null;
    this.byteCycle = null;
    this.nodeRows.clear();
    this.selection.setFrame(null);
    if (!id) {
      this.feedState = "closed";
      this.feedMessage = "";
      this.servedText = "select a pipeline in Decode";
      this.unsubscribe = () => {};
      this.view = { kind: "no-window" };
      this.renderAll();
      return;
    }
    this.servedText = "loading served address…";
    this.renderNote();
    void this.loadServedAddress(id);
    this.unsubscribe = subscribePipelineFeed(this.ctx, id, {
      frame: (f) => this.onFrame(f),
      state: (s, message) => { this.feedState = s; this.feedMessage = message; this.renderNote(); },
    });
    void this.loadBackfill(id);
    this.scheduleRefresh();
    this.renderAll();
  }

  /** The view window moved: re-fetch the window's recorded frames and re-derive the list. */
  private onWindowChanged() {
    if (this.pipelineId) void this.loadBackfill(this.pipelineId);
    this.scheduleRefresh();
  }

  private async loadBackfill(pipelineId: string) {
    const w = viewWindow(this.ctx.store.get());
    if (w === null) { this.backfill = null; return; }
    if (this.backfill && this.backfill.w.t0 === w.t0 && this.backfill.w.t1 === w.t1) return;
    const seq = ++this.backfillSeq;
    const loaded = await loadFrameBackfill(this.ctx.client, pipelineId, w);
    if (seq !== this.backfillSeq || pipelineId !== this.pipelineId) return;
    this.backfill = loaded;
    this.scheduleRefresh();
  }

  /** Coalesces a burst of arriving frames into one re-derivation; the live socket can deliver
   * faster than a list is worth re-rendering. */
  private scheduleRefresh() {
    if (this.refreshTimer !== null) return;
    this.refreshTimer = setTimeout(() => { this.refreshTimer = null; void this.refresh(); }, 200);
  }

  private async refresh() {
    const seq = ++this.viewSeq;
    const view = await resolveFrameList(this.ctx.client, this.ctx.store.get(), this.ring, this.backfill);
    if (seq !== this.viewSeq) return;
    this.view = view;
    this.renderFrameList();
  }

  private async loadServedAddress(id: string) {
    const seq = ++this.loadSeq;
    try {
      const streams = await this.ctx.client.get<StreamsResponse>("/api/streams");
      if (seq !== this.loadSeq) return;
      this.servedText = servedAddressText(streams, id);
    } catch {
      if (seq !== this.loadSeq) return;
      this.servedText = "served address unavailable";
    }
    this.renderNote();
  }

  private onFrame(f: FrameRecord) {
    const view = frameViewFromLive(f, this.ring.length);
    this.ring = pushRingFrame(this.ring, view);
    if (this.selection.get().frameSeq === null) this.selection.setFrame(view.index);
    this.scheduleRefresh();
  }

  private renderAll() {
    this.renderNote();
    this.renderFrameList();
  }

  private renderNote() {
    const feed = !this.pipelineId ? "" : this.feedState === "live" ? "live" : this.feedState === "reconnecting" ? `reconnecting${this.feedMessage ? ` (${this.feedMessage})` : ""}` : this.feedState === "connecting" ? "connecting…" : "closed";
    this.noteEl.textContent = inspectorNoteText(this.view, unplaceableFrames(this.ring), feed, this.servedText);
  }

  /** The window's frames, or `[]` — the one collection the list, the hex/tree panes and the note
   * all read (T-389's rule that two surfaces must never filter the same rows twice). */
  private frames(): RingFrame[] {
    return this.view.kind === "frames" ? this.view.frames : [];
  }

  private renderFrameList() {
    this.renderNote();
    const frames = this.frames();
    if (frames.length === 0) {
      this.frameListEl.replaceChildren(h("p", { class: "hint" }, frameListEmptyText(this.view as Exclude<FrameListView, { kind: "frames" }>)));
      this.renderBytesAndTree();
      return;
    }
    const rows = frameRowsVM(frames, this.selection.get().frameSeq);
    this.frameListEl.replaceChildren(...rows.map((r) => h(
      "div",
      {
        class: `insp-fr${r.bad ? " bad" : ""}${r.fresh ? " fresh" : ""}`,
        role: "listitem",
        "aria-current": String(r.selected),
        tabindex: "0",
        onclick: () => this.selection.setFrame(r.seq),
        onkeydown: (e: Event) => { const k = (e as KeyboardEvent).key; if (k === "Enter" || k === " ") { e.preventDefault(); this.selection.setFrame(r.seq); } },
      },
      h("span", { class: "t" }, r.timeText),
      h("span", { class: "c" }, r.channelText),
      h("span", { class: "s" }, r.summaryText),
    )));
    this.renderBytesAndTree();
  }

  private renderBytesAndTree() {
    const { frameSeq, fieldNodeId } = this.selection.get();
    const view = resolveSelectedFrame(this.frames(), frameSeq);
    this.nodeRows.clear();
    if (!view) {
      this.hexEl.replaceChildren(h("p", { class: "hint" }, "no frames yet."));
      this.flowEl.replaceChildren();
      this.treeEl.replaceChildren();
      return;
    }
    if (view.gated) {
      this.hexEl.replaceChildren(h("p", { class: "hint" }, "withheld: this content class doesn't offer frame bytes."));
      this.flowEl.replaceChildren();
      this.treeEl.replaceChildren();
      return;
    }
    const bytes = view.hex !== undefined ? hexBytes(view.hex) : [];
    const selRange = selectedByteRange(view.layers, fieldNodeId);
    this.hexEl.replaceChildren(...hexRows(bytes, view.layers, selRange).map((row) => h(
      "div", { class: "insp-hexrow" },
      h("span", { class: "off" }, row.offset),
      h("span", { class: "hx" }, ...row.cells.map((c) => h("b", { class: `${c.colorClass}${c.selected ? " sel" : ""}`, "data-b": c.byte, onclick: () => this.onByteClick(c.byte) }, c.hex))),
      h("span", {}),
      h("span", { class: "asc" }, ...row.cells.map((c) => h("b", { class: `${c.colorClass}${c.selected ? " sel" : ""}`, "data-b": c.byte, onclick: () => this.onByteClick(c.byte) }, c.ascii))),
    )));
    this.byteNoteEl.textContent = `${bytes.length} bytes · frame ${view.index}${view.timeS !== undefined ? ` at t=${view.timeS.toFixed(2)}s` : ""}`;
    this.flowEl.replaceChildren(this.pipelineId ? h("span", {}, "served as → ", h("code", {}, this.servedText)) : "");
    this.renderTree(view.layers, fieldNodeId);
  }

  // Nodes are already pre-order (stream-contract §14.2: "a node's children follow it") and `path`
  // is dotted from the root, so its segment count is the node's nesting depth: the tree renders as
  // one flat, indented list (as the mockup does) without needing to rebuild a hierarchy.
  private renderTree(tree: LayerTree | undefined, selectedId: number | null) {
    if (!tree || !tree.nodes.length) {
      this.treeEl.replaceChildren(h("p", { class: "hint" }, "no field map applied to this frame yet."));
      return;
    }
    const errs = errorsByPath(tree);
    this.treeEl.replaceChildren(...tree.nodes.map((n) => this.renderNode(n, errs, selectedId)));
  }

  private renderNode(n: LayerNode, errs: Map<string, LayerError[]>, selectedId: number | null): HTMLElement {
    const es = errs.get(n.path);
    const depth = Math.max(1, Math.min(n.path.split(".").length, 3));
    const row = h(
      "div",
      {
        class: `insp-tnode d${depth}${n.error ? " bad" : ""}`,
        tabindex: "0",
        "aria-current": String(n.id === selectedId),
        onclick: (e: Event) => { e.stopPropagation(); this.selection.setField(n.id); },
        onkeydown: (e: Event) => { const k = (e as KeyboardEvent).key; if (k === "Enter" || k === " ") { e.preventDefault(); e.stopPropagation(); this.selection.setField(n.id); } },
      },
      h("span", { class: "nm" }, h("span", { class: `dot ${nodeColorClass(n)}` }), n.label ?? n.name),
      h("span", { class: "vv" }, n.text ?? (es?.length ? es.map((x) => x.kind).join(", ") : "")),
      h("span", { class: "rg" }, `bytes ${n.bytes[0]}–${Math.max(n.bytes[0], n.bytes[1] - 1)}`),
    );
    this.nodeRows.set(n.id, row);
    return row;
  }

  private onByteClick(byte: number) {
    const view = resolveSelectedFrame(this.frames(), this.selection.get().frameSeq);
    const tree = view?.layers;
    if (!tree) return;
    const prev = this.byteCycle && this.byteCycle.byte === byte ? this.byteCycle.id : null;
    const id = cycleLeafAt(tree, byte, prev);
    this.byteCycle = id === null ? null : { byte, id };
    this.selection.setField(id);
    if (id !== null) this.nodeRows.get(id)?.scrollIntoView({ block: "nearest" });
  }
}

export const mountInspector: MountFn = (el, ctx) => { new InspectorPanel(el, ctx); };

/**
 * Mounts a packet inspector bound to one fixed pipeline, with its own independent frame/field
 * selection (T-195, "Added scope from docs/15 §7"): a per-signal output panel in Explore, reusing
 * every T-154 rendering/selection helper above rather than duplicating them. Returns a cleanup
 * function that closes the pipeline feed subscription — call it when the panel's tab is no longer
 * shown or the panel unmounts.
 */
export function mountInspectorPanel(el: HTMLElement, ctx: AppContext, pipelineId: string): () => void {
  const panel = new InspectorPanel(el, ctx, fixedPipeline(pipelineId), localSelection());
  return () => panel.destroy();
}
