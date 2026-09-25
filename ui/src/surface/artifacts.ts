// T-811 (MAP-11): the `artifacts` layer — image / harmonic / intermod relations drawn as LINKS.
//
// A backend `relation` of kind `artifact-of` says "this row is a receiver artefact of `source_id`".
// This layer draws a dashed line from the artefact's box to its source's box, in the one overlay
// pass (strokes only: the line is a run of tiny quads, since `overlay.ts` draws rectangles).
// Ranked evidence, never truth: the ink is the artefact grey and the line only exists where the
// backend served a claim AND both rows have a drawn presence interval. Nothing is inferred here —
// a claim whose source is not in view draws no line rather than a fabricated endpoint.
import type { Box } from "./lattice";
import type { OverlayQuad } from "./minimap";
import { toClip, type PaneRect } from "./surface";

const S_TO_NS = 1e9;

export interface LinkRow {
  readonly id: string;
  readonly state: string;
  readonly f_lo_hz: number;
  readonly f_hi_hz: number;
  readonly user_band?: { f_lo: number; f_hi: number } | null;
  readonly presence?: { last_interval?: { t_start_s: number; t_end_s: number; open: boolean } | null } | null;
  readonly relation?: { kind: string; artifact?: string | null; source_id?: string } | null;
}

/** Mechanism inks: image, harmonic, intermod — distinct from teal/lavender/grey/amber. */
export const LINK_INK: Readonly<Record<string, readonly [number, number, number, number]>> = {
  image: [0.95, 0.45, 0.65, 0.9],
  harmonic: [0.98, 0.6, 0.3, 0.9],
  intermod: [0.6, 0.85, 0.45, 0.9],
};
const LINK_DEFAULT: readonly [number, number, number, number] = [0.85, 0.85, 0.85, 0.85];

export interface Link {
  readonly id: string; // the artefact row
  readonly sourceId: string;
  readonly mechanism: string;
  readonly a: { fHz: number; tNs: number };
  readonly b: { fHz: number; tNs: number };
}

function centre(r: LinkRow, edgeNs: number): { fHz: number; tNs: number } | null {
  const iv = r.presence?.last_interval;
  if (!iv) return null;
  const band = r.user_band ?? null;
  const t0 = iv.t_start_s * S_TO_NS;
  const t1 = iv.open ? edgeNs : iv.t_end_s * S_TO_NS;
  return {
    fHz: band ? (band.f_lo + band.f_hi) / 2 : (r.f_lo_hz + r.f_hi_hz) / 2,
    tNs: (t0 + t1) / 2,
  };
}

/** Every artefact→source pair both of whose rows have a drawn interval. */
export function artifactLinks(rows: readonly LinkRow[], edgeNs: number): Link[] {
  const byId = new Map(rows.map((r) => [r.id, r]));
  const out: Link[] = [];
  for (const r of rows) {
    if (r.state !== "candidate" && r.state !== "confirmed") continue;
    const rel = r.relation;
    if (!rel || rel.kind !== "artifact-of" || !rel.source_id) continue;
    const src = byId.get(rel.source_id);
    if (!src) continue;
    const a = centre(r, edgeNs), b = centre(src, edgeNs);
    if (!a || !b) continue;
    out.push({ id: r.id, sourceId: src.id, mechanism: rel.artifact ?? "", a, b });
  }
  return out;
}

/** Dashed line quads per link through the pane's own mapping; a source-end square marks the source. */
export function artifactLinkQuads(
  links: readonly Link[], paneBox: Box, rect: PaneRect, dotPx = 2, stepPx = 7,
): OverlayQuad[] {
  const out: OverlayQuad[] = [];
  const hx = dotPx / Math.max(1, rect.w), hy = dotPx / Math.max(1, rect.h);
  for (const l of links) {
    const rgba = LINK_INK[l.mechanism] ?? LINK_DEFAULT;
    const box = (t: { fHz: number; tNs: number }) => {
      const [x0, y0, x1, y1] = toClip({ f0Hz: t.fHz, f1Hz: t.fHz + 1, t0Ns: t.tNs, t1Ns: t.tNs + 1 }, paneBox);
      return [x0, y0] as const;
    };
    const [ax, ay] = box(l.a), [bx, by] = box(l.b);
    const pxLen = Math.hypot((bx - ax) * rect.w / 2, (by - ay) * rect.h / 2);
    const n = Math.max(1, Math.ceil(pxLen / stepPx));
    const dot = (x: number, y: number, k: number) => {
      if (x < -1 || x > 1 || y < -1 || y > 1) return;
      out.push({ clip: [x - hx * k, y - hy * k, x + hx * k, y + hy * k], rgba, kind: "artifact-link", id: l.id });
    };
    for (let i = 0; i < n; i++) if (i % 2 === 0) dot(ax + (bx - ax) * (i / n), ay + (by - ay) * (i / n), 1);
    dot(bx, by, 2.5); // the source end
  }
  return out;
}
