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
import { selectInspectorField, selectInspectorFrame } from "./inspector-slice";
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

class InspectorPanel {
  private ring: RingFrame[] = [];
  private unsubscribe: () => void = () => {};
  private pipelineId: string | null = null;
  private loadSeq = 0;
  private feedState: FeedState = "closed";
  private feedMessage = "";
  private servedText = "select a pipeline in Decode";
  private byteCycle: { byte: number; id: number } | null = null;
  private nodeRows = new Map<number, HTMLElement>();

  private frameListEl: HTMLElement;
  private noteEl: HTMLElement;
  private byteNoteEl: HTMLElement;
  private hexEl: HTMLElement;
  private flowEl: HTMLElement;
  private treeEl: HTMLElement;

  constructor(el: HTMLElement, private ctx: AppContext) {
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
    ctx.store.select((s) => s.decode.pipelineId, (id) => this.onPipeline(id), { immediate: true });
    ctx.store.select((s) => s.inspector.frameSeq, () => this.renderFrameList());
    ctx.store.select((s) => s.inspector.fieldNodeId, () => this.renderBytesAndTree());
  }

  private onPipeline(id: string | null) {
    this.unsubscribe();
    this.pipelineId = id;
    this.ring = [];
    this.byteCycle = null;
    this.nodeRows.clear();
    this.ctx.store.set(selectInspectorFrame(null));
    if (!id) {
      this.feedState = "closed";
      this.feedMessage = "";
      this.servedText = "select a pipeline in Decode";
      this.unsubscribe = () => {};
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
    this.renderAll();
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
    if (this.ctx.store.get().inspector.frameSeq === null) this.ctx.store.set(selectInspectorFrame(view.index));
    else this.renderFrameList();
  }

  private renderAll() {
    this.renderNote();
    this.renderFrameList();
  }

  private renderNote() {
    const feed = !this.pipelineId ? "" : this.feedState === "live" ? "live" : this.feedState === "reconnecting" ? `reconnecting${this.feedMessage ? ` (${this.feedMessage})` : ""}` : this.feedState === "connecting" ? "connecting…" : "closed";
    this.noteEl.textContent = feed ? `${feed} · ${this.servedText}` : this.servedText;
  }

  private renderFrameList() {
    const rows = frameRowsVM(this.ring, this.ctx.store.get().inspector.frameSeq);
    this.frameListEl.replaceChildren(...rows.map((r) => h(
      "div",
      {
        class: `insp-fr${r.bad ? " bad" : ""}${r.fresh ? " fresh" : ""}`,
        role: "listitem",
        "aria-current": String(r.selected),
        tabindex: "0",
        onclick: () => this.ctx.store.set(selectInspectorFrame(r.seq)),
        onkeydown: (e: Event) => { const k = (e as KeyboardEvent).key; if (k === "Enter" || k === " ") { e.preventDefault(); this.ctx.store.set(selectInspectorFrame(r.seq)); } },
      },
      h("span", { class: "t" }, r.timeText),
      h("span", { class: "c" }, r.channelText),
      h("span", { class: "s" }, r.summaryText),
    )));
    this.renderBytesAndTree();
  }

  private renderBytesAndTree() {
    const { frameSeq, fieldNodeId } = this.ctx.store.get().inspector;
    const view = resolveSelectedFrame(this.ring, frameSeq);
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
        onclick: (e: Event) => { e.stopPropagation(); this.ctx.store.set(selectInspectorField(n.id)); },
        onkeydown: (e: Event) => { const k = (e as KeyboardEvent).key; if (k === "Enter" || k === " ") { e.preventDefault(); e.stopPropagation(); this.ctx.store.set(selectInspectorField(n.id)); } },
      },
      h("span", { class: "nm" }, h("span", { class: `dot ${nodeColorClass(n)}` }), n.label ?? n.name),
      h("span", { class: "vv" }, n.text ?? (es?.length ? es.map((x) => x.kind).join(", ") : "")),
      h("span", { class: "rg" }, `bytes ${n.bytes[0]}–${Math.max(n.bytes[0], n.bytes[1] - 1)}`),
    );
    this.nodeRows.set(n.id, row);
    return row;
  }

  private onByteClick(byte: number) {
    const view = resolveSelectedFrame(this.ring, this.ctx.store.get().inspector.frameSeq);
    const tree = view?.layers;
    if (!tree) return;
    const prev = this.byteCycle && this.byteCycle.byte === byte ? this.byteCycle.id : null;
    const id = cycleLeafAt(tree, byte, prev);
    this.byteCycle = id === null ? null : { byte, id };
    this.ctx.store.set(selectInspectorField(id));
    if (id !== null) this.nodeRows.get(id)?.scrollIntoView({ block: "nearest" });
  }
}

export const mountInspector: MountFn = (el, ctx) => { new InspectorPanel(el, ctx); };
