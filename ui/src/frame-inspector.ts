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
  /**
   * Integer Unix nanoseconds (stream-contract §5.1). The name carries the unit since contract
   * 1.2 (T-354); past `Number.MAX_SAFE_INTEGER`, so `JSON.parse` has already rounded it to about
   * ¼ µs — fine for placing a frame on a time axis, never an exact instant.
   */
  t_ns?: number;
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
    timeS: f.t_ns !== undefined ? f.t_ns / 1e9 : undefined,
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

