// M1 Inspector UI (T-090, SIGNAL-062): frame list, hex+ASCII, layer tree, bidirectional linked
// selection, over docs/api.md "Inspector (T-089)" (`POST /api/inspector/parse`,
// `POST /api/captures/{id}/parse`) and docs/stream-contract.md §14.2. Thin client: every byte and
// bit range, value, unit and fit/error comes from the server response; this file does no range
// arithmetic or parsing of its own — only pagination bookkeeping and DOM presentation over
// already-known API state (paging offsets, tree nesting, click-to-highlight indices).
//
// Live streaming (`/ws/open/inspector`) is not served yet (docs/api.md "Decoder workbench
// (planned, M1)"): TODO once T-088/T-092 land, open it here as a third input-source mode
// alongside "capture" and "paste frames".

// ---- Wire types (docs/api.md "Inspector (T-089)", docs/stream-contract.md §14.2) ----

export type FitStatus = "none" | "ok" | "partial" | "failed";

export interface LayerError {
  path: string;
  kind: "out-of-bounds" | "bad-length" | "missing-reference" | "repeat-limit" | "node-limit" | "parity";
  need_bits?: number;
  have_bits?: number;
}

export interface LayerNode {
  id: number;
  parent?: number;
  name: string;
  path: string;
  type: "layer" | "uint" | "int" | "enum" | "ascii" | "bitfield" | "flag" | "bytes";
  bits: [number, number];
  bytes: [number, number];
  value?: number | string | boolean;
  text?: string;
  label?: string;
  error?: boolean;
}

export interface LayerTree {
  nodes: LayerNode[];
  byte_index: number[][];
  fit: FitStatus;
  errors?: LayerError[];
}

export interface FrameMetadataDto {
  frame?: number;
  sample_index?: number;
  channel?: number;
  channel_hz?: number;
  bit_len?: number;
  recipe_version?: number;
  edit_rev?: number;
  fec_corrected_bits?: number;
  fit?: FitStatus;
}

export interface FrameRecordDto {
  type?: string;
  seq?: number;
  t?: number; // nanoseconds since epoch (stream-contract §5.1)
  content_class?: string;
  gated: boolean;
  crc_status?: "valid" | "invalid" | "no-crc" | "unknown";
  decoder?: string;
  frame_model?: string;
  emitter_id?: string;
  metadata: FrameMetadataDto;
  content?: { hex: string; layers?: LayerTree };
}

export interface FitSummary {
  frames: number;
  ok: number;
  partial: number;
  failed: number;
  unparsed: number;
  errors: Record<string, Record<string, number>>;
  truncated?: boolean;
}

export interface CaptureParseResponse {
  capture_id: string;
  stream: { stream_id?: string; content_class?: string; message_schema?: string; inspector?: { source: { kind: string; capture_id?: string; reparse?: boolean } } };
  total_frames: number;
  from_frame: number;
  limit: number;
  next_from_frame: number | null;
  frames: FrameRecordDto[];
  fit: FitSummary | null;
}

export interface InlineParseFrame { hex: string; bit_len?: number }
export interface InlineParseResponseFrame { bit_len: number; hex: string; layers: LayerTree }
export interface InlineParseResponse { frames: InlineParseResponseFrame[]; fit: FitSummary }

/** Default page size (docs/api.md: 1-500, default 100 for captures; we page smaller for a
 * readable frame list). */
export const PAGE_LIMIT = 50;
/** `POST /api/inspector/parse` accepts 1-500 pasted frames. */
export const MAX_INLINE_FRAMES = 500;

// ---- View model: capture-page and inline-parse responses render through the same shape ----

export interface FrameView {
  index: number;
  timeS?: number;
  channel?: number;
  channelHz?: number;
  bitLen: number;
  crcStatus?: string;
  fit: FitStatus;
  hex?: string;
  layers?: LayerTree;
  gated?: boolean;
}

export function frameViewFromCapture(f: FrameRecordDto, fallbackIndex: number): FrameView {
  return {
    index: f.metadata?.frame ?? fallbackIndex,
    timeS: f.t !== undefined ? f.t / 1e9 : undefined,
    channel: f.metadata?.channel,
    channelHz: f.metadata?.channel_hz,
    bitLen: f.metadata?.bit_len ?? 0,
    crcStatus: f.crc_status,
    fit: f.metadata?.fit ?? "none",
    hex: f.content?.hex,
    layers: f.content?.layers,
    gated: f.gated,
  };
}

export function frameViewFromInline(f: InlineParseResponseFrame, index: number): FrameView {
  return { index, bitLen: f.bit_len, fit: f.layers?.fit ?? "none", hex: f.hex, layers: f.layers, gated: false };
}

// ---- Layer tree: nesting, error lookup, linked selection ----

export interface TreeNode { node: LayerNode; children: TreeNode[] }

/** Nests the pre-order flat node list into a tree via each node's `parent` id. Structural only
 * (rendering/indentation), not signal logic: the ranges, values and units all come from the API. */
export function buildTree(nodes: readonly LayerNode[]): TreeNode[] {
  const byId = new Map<number, TreeNode>();
  const roots: TreeNode[] = [];
  for (const n of nodes) {
    const tn: TreeNode = { node: n, children: [] };
    byId.set(n.id, tn);
    const parent = n.parent !== undefined ? byId.get(n.parent) : undefined;
    if (parent) parent.children.push(tn);
    else roots.push(tn);
  }
  return roots;
}

export function nodeById(tree: LayerTree, id: number): LayerNode | null {
  return tree.nodes.find((n) => n.id === id) ?? null;
}

/** Groups `layers.errors` by dotted field path, for the tree to show next to each node. */
export function errorsByPath(tree: LayerTree): Map<string, LayerError[]> {
  const m = new Map<string, LayerError[]>();
  for (const e of tree.errors ?? []) {
    const list = m.get(e.path);
    if (list) list.push(e); else m.set(e.path, [e]);
  }
  return m;
}

/**
 * Byte `b` -> the leaf field to select (stream-contract §14.2 "Linked selection"): the byte's
 * first overlapping leaf, or the next one after `current` when `current` is already selected for
 * this same byte (repeat clicks cycle through `byte_index[b]`). Returns null for a byte with no
 * leaf (padding, or bytes outside any field).
 */
export function cycleLeafAt(tree: LayerTree, byte: number, current: number | null): number | null {
  const ids = tree.byte_index[byte] ?? [];
  if (!ids.length) return null;
  if (current === null) return ids[0];
  const i = ids.indexOf(current);
  return i < 0 ? ids[0] : ids[(i + 1) % ids.length];
}

// ---- Hex + ASCII ----

export function hexBytes(hex: string): number[] {
  const out: number[] = [];
  for (let i = 0; i + 1 < hex.length; i += 2) out.push(parseInt(hex.slice(i, i + 2), 16));
  return out;
}

export function asciiChar(byte: number): string {
  return byte >= 0x20 && byte < 0x7f ? String.fromCharCode(byte) : "·";
}

// ---- Fit summary ----

export function fitSummaryText(fit: FitSummary): string {
  const parts = [`${fit.frames} frames`, `${fit.ok} ok`, `${fit.partial} partial`, `${fit.failed} failed`, `${fit.unparsed} unparsed`];
  const errs = Object.entries(fit.errors);
  if (errs.length) {
    parts.push(errs.map(([path, kinds]) => `${path}: ${Object.entries(kinds).map(([k, c]) => `${k}×${c}`).join(", ")}`).join("; "));
  }
  if (fit.truncated) parts.push("truncated to the first 100000 frames");
  return parts.join(" · ");
}

export function fitClass(fit: string | undefined): string {
  return fit === "ok" ? "ok" : fit === "partial" ? "warn" : fit === "failed" ? "bad" : "";
}

export function crcClass(status: string | undefined): string {
  return status === "valid" ? "ok" : status === "invalid" ? "bad" : "";
}

// ---- Paging (client-side bookkeeping over already-known offsets, not signal logic) ----

/** The previous page's `from_frame`: the server gives `next_from_frame` but no `prev_from_frame`. */
export function pagePrevFrom(fromFrame: number, limit: number): number {
  return Math.max(0, fromFrame - limit);
}

// ---- Draft field map + pasted frames (the authoring flow before capture storage, T-089) ----

/** Parses the field-map textarea: empty means "no field map" (plain frame/hex listing). */
export function parseFieldMapInput(text: string): { value?: unknown; error?: string } {
  const t = text.trim();
  if (!t) return {};
  try {
    return { value: JSON.parse(t) };
  } catch (e) {
    return { error: `invalid JSON: ${(e as Error).message}` };
  }
}

/** Parses the "paste frames" textarea: one hex frame per non-empty, non-`#`-comment line. */
export function parsePastedFrames(text: string): { frames: InlineParseFrame[]; error?: string } {
  const lines = text.split("\n").map((l) => l.trim()).filter((l) => l && !l.startsWith("#"));
  if (!lines.length) return { frames: [], error: "paste at least one frame as hex bytes" };
  if (lines.length > MAX_INLINE_FRAMES) return { frames: [], error: `${MAX_INLINE_FRAMES} frames max per request` };
  for (const l of lines) {
    if (!/^[0-9a-fA-F]+$/.test(l) || l.length % 2 !== 0) return { frames: [], error: `not even-length hex: "${l}"` };
  }
  return { frames: lines.map((hex) => ({ hex })) };
}

// ---- API calls (thin wrappers so the pure request/response shape is unit-tested without DOM) ----

export interface InspectorClient {
  post<T = unknown>(path: string, body?: unknown): Promise<T>;
}

function captureParseBody(fieldMap: unknown, fromFrame: number, limit: number): Record<string, unknown> {
  const body: Record<string, unknown> = { from_frame: fromFrame, limit };
  if (fieldMap !== undefined) body.field_map = fieldMap;
  return body;
}

export async function loadCapturePage(client: InspectorClient, captureId: string, fieldMap: unknown, fromFrame: number, limit: number): Promise<CaptureParseResponse> {
  return client.post<CaptureParseResponse>(`/api/captures/${encodeURIComponent(captureId)}/parse`, captureParseBody(fieldMap, fromFrame, limit));
}

export async function parseInlineFrames(client: InspectorClient, fieldMap: unknown, frames: InlineParseFrame[]): Promise<InlineParseResponse> {
  return client.post<InlineParseResponse>("/api/inspector/parse", { field_map: fieldMap, frames });
}

// ---- Decoded captures (T-092, docs/api.md "Decoded captures"): list + frame/time scrub ----

/** One always-on recording of a pipeline's decoded stream (`GET /api/captures`). */
export interface CaptureInfoDto {
  id: string;
  pipeline_id: string;
  recipe_id: string;
  recipe_version: number;
  output_id: string;
  stream_id: string;
  content_class: string;
  segment: number;
  started: number;
  ended: number | null;
  t_first: number | null;
  t_last: number | null;
  frames: number;
  bytes: number;
  dropped_records: number;
  recording: boolean;
  end_reason: string | null;
}

/** `GET /api/captures/{id}/frames` page. */
export interface CaptureFramesResponse {
  capture_id: string;
  total_frames: number;
  from_frame: number;
  limit: number;
  next_from_frame: number | null;
  frames: FrameRecordDto[];
}

export interface CaptureListClient {
  get<T = unknown>(path: string): Promise<T>;
}

export async function listCaptures(client: CaptureListClient): Promise<CaptureInfoDto[]> {
  const r = await client.get<{ captures?: CaptureInfoDto[] }>("/api/captures");
  return r.captures ?? [];
}

export function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MiB`;
}

/** Picker label: recipe revision, pipeline/output, segment, size, and whether it still records. */
export function captureLabel(c: CaptureInfoDto): string {
  const seg = c.segment ? ` #${c.segment}` : "";
  const live = c.recording ? " · recording" : "";
  return `${c.recipe_id}@${c.recipe_version} · ${c.pipeline_id}/${c.output_id}${seg} · ${c.frames} frames · ${fmtBytes(c.bytes)}${live}`;
}

/** The page start for a scrub-slider position: the scrubbed frame, clamped into the recording. */
export function scrubPageStart(frame: number, total: number): number {
  if (!Number.isFinite(frame) || total <= 0) return 0;
  return Math.min(Math.max(0, Math.floor(frame)), total - 1);
}

/** Unix seconds for an offset from the capture's first frame (its start when it has none). */
export function seekTimeS(c: Pick<CaptureInfoDto, "t_first" | "started">, offsetS: number): number {
  return (c.t_first ?? c.started) + Math.max(0, Number.isFinite(offsetS) ? offsetS : 0);
}

/** The first frame at or after `tS` (Unix seconds): the server searches the capture's frame index. */
export async function frameAtTime(client: CaptureListClient, captureId: string, tS: number): Promise<number> {
  const r = await client.get<CaptureFramesResponse>(`/api/captures/${encodeURIComponent(captureId)}/frames?from_t=${tS}&limit=1`);
  return r.from_frame;
}

// ---- DOM wiring (untested under node:test, like the rest of ui/src's panels: see ui/test's own
// note in inventory.test.ts. Only pure functions above are imported by tests). ----

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

function errText(e: unknown): string {
  const anyE = e as { code?: unknown };
  const code = anyE && typeof anyE.code === "string" ? anyE.code : null;
  const msg = e instanceof Error ? e.message : String(e);
  return code ? `${msg} (${code})` : msg;
}

function fmtTimeS(s: number | undefined): string {
  if (s === undefined || !Number.isFinite(s)) return "—";
  return new Date(s * 1000).toISOString().replace("T", " ").slice(0, 23) + "Z";
}

/** The frame inspector pane: capture id + paging, a draft field map, pasted-frame parsing, a
 * frame list, and bidirectional linked selection between the hex+ASCII pane and the layer tree. */
export class FrameInspectorPanel {
  private mode: "capture" | "paste" = "capture";
  private captureId = "";
  private fromFrame = 0;
  private page: CaptureParseResponse | InlineParseResponse | null = null;
  private views: FrameView[] = [];
  private selected: FrameView | null = null;
  private selectedNodeId: number | null = null;
  private byteCycle: { byte: number; id: number } | null = null;
  private nodeRows = new Map<number, HTMLElement>();
  private captures = new Map<string, CaptureInfoDto>();
  private total = 0;

  constructor(private client: InspectorClient & CaptureListClient) {
    $("fi-capture-form").addEventListener("submit", (e) => { e.preventDefault(); this.fromFrame = 0; void this.loadCapture(); });
    // T-092: pick a recorded stream, scrub by frame, or jump to a time (the server searches its index).
    $("fi-capture-refresh").addEventListener("click", () => void this.refreshCaptures());
    $<HTMLSelectElement>("fi-capture-list").addEventListener("change", (e) => {
      const id = (e.target as HTMLSelectElement).value;
      if (!id) return;
      $<HTMLInputElement>("fi-capture-id").value = id;
      this.fromFrame = 0;
      void this.loadCapture();
    });
    const scrub = $<HTMLInputElement>("fi-scrub");
    scrub.addEventListener("input", () => { $("fi-scrub-info").textContent = `frame ${scrub.value} of ${this.total}`; });
    scrub.addEventListener("change", () => { this.fromFrame = scrubPageStart(Number(scrub.value), this.total); void this.loadCapture(); });
    $("fi-seek-form").addEventListener("submit", (e) => { e.preventDefault(); void this.seek(); });
    void this.refreshCaptures();
    $("fi-prev").addEventListener("click", () => { this.fromFrame = pagePrevFrom(this.fromFrame, PAGE_LIMIT); void this.loadCapture(); });
    $("fi-next").addEventListener("click", () => {
      const p = this.page as CaptureParseResponse | null;
      if (p && "next_from_frame" in p && p.next_from_frame !== null) { this.fromFrame = p.next_from_frame; void this.loadCapture(); }
    });
    $("fi-map-apply").addEventListener("click", () => { this.mode = "capture"; this.fromFrame = 0; void this.loadCapture(); });
    $("fi-paste-parse").addEventListener("click", () => void this.loadPasted());
  }

  private fieldMap(): { value?: unknown; error?: string } {
    return parseFieldMapInput($<HTMLTextAreaElement>("fi-field-map").value);
  }

  private setError(msg: string) {
    $("fi-error").textContent = msg;
  }

  async loadCapture() {
    this.setError("");
    const id = $<HTMLInputElement>("fi-capture-id").value.trim();
    if (!id) { this.setError("enter a capture id"); return; }
    this.captureId = id;
    this.mode = "capture";
    const fm = this.fieldMap();
    if (fm.error) { this.setError(fm.error); return; }
    $("fi-status").textContent = "loading…";
    try {
      const page = await loadCapturePage(this.client, id, fm.value, this.fromFrame, PAGE_LIMIT);
      this.page = page;
      this.views = page.frames.map((f, i) => frameViewFromCapture(f, this.fromFrame + i));
      $("fi-status").textContent = `${page.capture_id} · ${page.total_frames} frames total`;
      $("fi-fit").textContent = page.fit ? fitSummaryText(page.fit) : "";
      $<HTMLButtonElement>("fi-prev").disabled = this.fromFrame <= 0;
      $<HTMLButtonElement>("fi-next").disabled = page.next_from_frame === null;
      $("fi-page-info").textContent = `frames ${page.from_frame}–${page.from_frame + page.frames.length - 1} of ${page.total_frames}`;
      this.total = page.total_frames;
      const scrub = $<HTMLInputElement>("fi-scrub");
      scrub.max = String(Math.max(0, page.total_frames - 1));
      scrub.value = String(page.from_frame);
      scrub.disabled = page.total_frames <= 0;
      $("fi-scrub-info").textContent = `frame ${page.from_frame} of ${page.total_frames}`;
      $<HTMLButtonElement>("fi-seek-go").disabled = page.total_frames <= 0;
      this.renderFrameList();
      this.selectFrame(this.views[0] ?? null);
    } catch (e) {
      this.setError(errText(e));
      $("fi-status").textContent = "";
    }
  }

  /** Refills the recorded-stream picker from `GET /api/captures` (newest first). */
  private async refreshCaptures() {
    try {
      const list = await listCaptures(this.client);
      this.captures = new Map(list.map((c) => [c.id, c]));
      const sel = $<HTMLSelectElement>("fi-capture-list");
      const keep = sel.value || this.captureId;
      const none = document.createElement("option");
      none.value = "";
      none.textContent = list.length ? "(pick a recording)" : "(no recordings yet)";
      sel.replaceChildren(none, ...list.map((c) => {
        const o = document.createElement("option");
        o.value = c.id;
        o.textContent = captureLabel(c);
        return o;
      }));
      sel.value = keep && this.captures.has(keep) ? keep : "";
    } catch (e) {
      this.setError(errText(e));
    }
  }

  /** Jumps the page to the first frame at or after "seconds from capture start". */
  private async seek() {
    this.setError("");
    const id = this.captureId;
    if (!id) { this.setError("load a capture first"); return; }
    let c = this.captures.get(id);
    if (!c) { await this.refreshCaptures(); c = this.captures.get(id); }
    if (!c) { this.setError("that capture is not in the recorded list"); return; }
    try {
      const offset = Number($<HTMLInputElement>("fi-seek-s").value);
      this.fromFrame = await frameAtTime(this.client, id, seekTimeS(c, offset));
      await this.loadCapture();
    } catch (e) {
      this.setError(errText(e));
    }
  }

  private async loadPasted() {
    this.setError("");
    const parsed = parsePastedFrames($<HTMLTextAreaElement>("fi-paste-frames").value);
    if (parsed.error) { this.setError(parsed.error); return; }
    const fm = this.fieldMap();
    if (fm.error) { this.setError(fm.error); return; }
    if (fm.value === undefined) { this.setError("a field map is required to parse pasted frames"); return; }
    this.mode = "paste";
    $("fi-status").textContent = "parsing…";
    try {
      const res = await parseInlineFrames(this.client, fm.value, parsed.frames);
      this.page = res;
      this.views = res.frames.map((f, i) => frameViewFromInline(f, i));
      $("fi-status").textContent = `${this.views.length} pasted frame${this.views.length === 1 ? "" : "s"}`;
      $("fi-fit").textContent = fitSummaryText(res.fit);
      $<HTMLButtonElement>("fi-prev").disabled = true;
      $<HTMLButtonElement>("fi-next").disabled = true;
      $("fi-page-info").textContent = "";
      this.renderFrameList();
      this.selectFrame(this.views[0] ?? null);
    } catch (e) {
      this.setError(errText(e));
      $("fi-status").textContent = "";
    }
  }

  private renderFrameList() {
    const body = $("fi-frame-body");
    body.replaceChildren(...this.views.map((v) => this.frameRow(v)));
    $("fi-frame-table").hidden = !this.views.length;
  }

  private frameRow(v: FrameView): HTMLTableRowElement {
    const tr = document.createElement("tr");
    const td = (text: string, cls = "") => { const c = document.createElement("td"); c.textContent = text; if (cls) c.className = cls; tr.append(c); return c; };
    td(String(v.index), "num");
    td(fmtTimeS(v.timeS));
    td(v.channel !== undefined ? String(v.channel) : "—", "num opt");
    td(String(v.bitLen), "num");
    const crc = td(v.crcStatus ?? "—");
    if (v.crcStatus) crc.className = crcClass(v.crcStatus);
    const fit = td(v.gated ? "gated" : v.fit);
    fit.className = v.gated ? "warn" : fitClass(v.fit);
    if (this.selected === v) tr.className = "sel";
    tr.addEventListener("click", () => this.selectFrame(v));
    return tr;
  }

  private selectFrame(v: FrameView | null) {
    this.selected = v;
    this.selectedNodeId = null;
    this.byteCycle = null;
    for (const tr of $("fi-frame-body").querySelectorAll("tr")) tr.classList.remove("sel");
    const idx = v ? this.views.indexOf(v) : -1;
    if (idx >= 0) $("fi-frame-body").children[idx]?.classList.add("sel");
    this.renderHex();
    this.renderTree();
  }

  private renderHex() {
    const box = $("fi-hex");
    box.replaceChildren();
    const v = this.selected;
    if (!v) return;
    if (v.gated) { const p = document.createElement("p"); p.className = "hint"; p.textContent = "gated: content withheld by the stream's class."; box.append(p); return; }
    if (v.hex === undefined) { const p = document.createElement("p"); p.className = "hint"; p.textContent = "no bytes for this frame."; box.append(p); return; }
    const bytes = hexBytes(v.hex);
    for (let row = 0; row < bytes.length; row += 8) {
      const chunk = bytes.slice(row, row + 8);
      const line = document.createElement("div");
      line.className = "fi-hex-row";
      const off = document.createElement("span");
      off.className = "fi-offset";
      off.textContent = row.toString(16).padStart(4, "0");
      const hexes = document.createElement("span");
      hexes.className = "fi-hex-bytes";
      const asciis = document.createElement("span");
      asciis.className = "fi-ascii";
      chunk.forEach((b, i) => {
        const byteIndex = row + i;
        const h = document.createElement("span");
        h.className = "fi-byte";
        h.textContent = b.toString(16).padStart(2, "0");
        h.dataset.byte = String(byteIndex);
        h.addEventListener("click", () => this.onByteClick(byteIndex));
        hexes.append(h);
        const a = document.createElement("span");
        a.className = "fi-ascii-ch";
        a.textContent = asciiChar(b);
        a.dataset.byte = String(byteIndex);
        a.addEventListener("click", () => this.onByteClick(byteIndex));
        asciis.append(a);
      });
      line.append(off, hexes, asciis);
      box.append(line);
    }
  }

  private renderTree() {
    const box = $("fi-tree");
    box.replaceChildren();
    this.nodeRows.clear();
    const v = this.selected;
    const layers = v?.layers;
    if (!v) return;
    if (v.gated) { const p = document.createElement("p"); p.className = "hint"; p.textContent = "gated: layers withheld by the stream's class."; box.append(p); return; }
    if (!layers) { const p = document.createElement("p"); p.className = "hint"; p.textContent = "no field map applied to this frame yet."; box.append(p); return; }
    const errs = errorsByPath(layers);
    for (const tn of buildTree(layers.nodes)) box.append(this.renderNode(tn, layers, errs));
  }

  private renderNode(tn: TreeNode, tree: LayerTree, errs: Map<string, LayerError[]>): HTMLElement {
    const n = tn.node;
    const row = document.createElement("div");
    row.className = `fi-field${n.error ? " bad" : ""}`;
    row.dataset.nodeId = String(n.id);
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = n.label ?? n.name;
    const type = document.createElement("span");
    type.className = "hint";
    type.textContent = n.type;
    row.append(name, type);
    if (n.text !== undefined) { const val = document.createElement("span"); val.textContent = n.text; row.append(val); }
    const es = errs.get(n.path);
    if (es?.length) { const e = document.createElement("span"); e.className = "hint bad"; e.textContent = es.map((x) => x.kind).join(", "); row.append(e); }
    row.addEventListener("click", (ev) => { ev.stopPropagation(); this.onFieldClick(n); });
    this.nodeRows.set(n.id, row);
    if (!tn.children.length) return row;
    const det = document.createElement("details");
    det.open = true;
    const sum = document.createElement("summary");
    sum.append(row);
    det.append(sum);
    for (const c of tn.children) det.append(this.renderNode(c, tree, errs));
    return det;
  }

  private onFieldClick(n: LayerNode) {
    this.byteCycle = null;
    this.selectNode(n.id);
    this.highlightBytes(n.bytes);
  }

  private onByteClick(byte: number) {
    const layers = this.selected?.layers;
    if (!layers) return;
    const prev = this.byteCycle && this.byteCycle.byte === byte ? this.byteCycle.id : null;
    const id = cycleLeafAt(layers, byte, prev);
    this.byteCycle = id === null ? null : { byte, id };
    if (id === null) { this.highlightBytes([byte, byte + 1]); this.selectNode(null); return; }
    const n = nodeById(layers, id);
    this.selectNode(id);
    if (n) this.highlightBytes(n.bytes);
    const el = this.nodeRows.get(id);
    if (el) { let d: HTMLElement | null = el.closest("details"); while (d) { (d as HTMLDetailsElement).open = true; d = d.parentElement?.closest("details") ?? null; } el.scrollIntoView({ block: "nearest" }); }
  }

  private selectNode(id: number | null) {
    if (this.selectedNodeId !== null) this.nodeRows.get(this.selectedNodeId)?.classList.remove("selected");
    this.selectedNodeId = id;
    if (id !== null) this.nodeRows.get(id)?.classList.add("selected");
  }

  private highlightBytes([first, end]: [number, number]) {
    for (const el of $("fi-hex").querySelectorAll<HTMLElement>(".fi-byte, .fi-ascii-ch")) {
      const b = Number(el.dataset.byte);
      el.classList.toggle("hl", b >= first && b < end);
    }
  }
}
