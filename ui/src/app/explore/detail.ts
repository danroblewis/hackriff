// T-804 (MAP-04): the detail sheet's view-model — what the focused signal's sheet says, as pure
// functions over fields `/api/inventory` already served (docs/23 §10.3, ui/mockups/map-ui-v1.html
// `renderDetail`). The sheet itself is the T-803 bottom sheet; `explore/index.ts` renders these
// lines into it.
//
// THIN CLIENT (CLAUDE.md): nothing here measures, ranks or classifies. The measurement and its time
// are the row's `measured` object (T-350), liveness and time extent are `presence` (T-284/T-410),
// the explanations are the server's ranked list in the server's order. The only arithmetic is
// formatting served instants against the capture clock's live edge ("4 s ago") — the same
// presentation arithmetic as the time↔pixel mapping.
import type { Row } from "./inventory";

/** docs/api.md `/api/inventory` `measured` (T-350): one detection's levels **and** the time range it
 * was measured over, so a level is never read without its time. `null` exactly when nothing linked
 * has been measured yet. */
export interface Measured {
  snr_db: number; peak_dbfs: number;
  t_start_s: number; t_end_s: number; duration_s: number;
}

/** A row as the detail sheet reads it: `measured` is optional so fixtures predating T-350 still
 * typecheck; `undefined` is treated exactly like `null` (nothing measured is claimed). */
export type DetailRow = Row & { measured?: Measured | null };

/** A duration in words. A one-off burst reads in milliseconds, never rounded to "0 s" — an
 * ephemeral emission is first-class (ADR-0017). */
export function spanText(s: number): string {
  if (!Number.isFinite(s) || s < 0) return "—";
  if (s < 1) return `${Math.round(s * 1000)} ms`;
  if (s < 60) return `${s < 10 ? s.toFixed(1) : Math.round(s)} s`;
  if (s < 3600) return `${Math.round(s / 60)} min`;
  return `${(s / 3600).toFixed(1)} h`;
}

/** An absolute capture instant as a UTC clock, `HH:MM:SS` (the capture clock is absolute Unix time;
 * a replay's instants are its recording's own, so this names *when on the recording*). */
export function clockText(tS: number): string {
  if (!Number.isFinite(tS)) return "—";
  return `${new Date(tS * 1000).toISOString().slice(11, 19)} UTC`;
}

/** `tS` relative to the live edge `edgeS` ("12 s ago"), or `null` when the live edge is unknown —
 * never measured against the browser's clock (T-379: a replay runs on a clock of its own). */
export function agoText(tS: number, edgeS: number | null): string | null {
  if (edgeS === null || !Number.isFinite(edgeS) || !Number.isFinite(tS)) return null;
  const d = edgeS - tS;
  return d <= 0.5 ? "just now" : `${spanText(d)} ago`;
}

/** The big frequency and the bandwidth the sheet leads with: the output-refined tuning when the
 * server has one (T-070), else the blind measurement — both served, neither computed here. */
export function detailFreq(r: Pick<Row, "f_center_hz" | "bandwidth_hz" | "refined">): { centerHz: number; bandwidthHz: number } {
  return { centerHz: r.refined?.center_hz ?? r.f_center_hz, bandwidthHz: r.refined?.bandwidth_hz ?? r.bandwidth_hz };
}

export type LivenessKind = "live" | "ended" | "absent" | "unreported";

/**
 * The liveness line (ADR-0017/0019): a signal is `[start, end?]`, ongoing until an end is
 * affirmatively detected, and a detected end is provisional. So `live` says since when; `ended`
 * says when and that the end is revocable; `absent` says the window holds no interval; a row with no
 * `presence` at all (a server predating T-284) says nothing was reported rather than inventing one.
 */
export function livenessLine(r: Pick<Row, "presence">, edgeS: number | null): { kind: LivenessKind; text: string } {
  const p = r.presence;
  if (!p) return { kind: "unreported", text: "Liveness not reported by this server" };
  if (p.liveness === "live") {
    const since = p.last_interval ? agoText(p.last_interval.t_start_s, edgeS) : null;
    const at = p.last_interval ? clockText(p.last_interval.t_start_s) : null;
    return { kind: "live", text: `On air${at ? ` · since ${at}` : ""}${since ? ` (${since})` : ""}` };
  }
  if (p.liveness === "ended") {
    const endS = p.ended_t_s ?? p.last_interval?.t_end_s ?? null;
    const ago = endS === null ? null : agoText(endS, edgeS);
    return {
      kind: "ended",
      text: `Ended${endS === null ? "" : ` ${clockText(endS)}`}${ago ? ` (${ago})` : ""} — provisional, reopens if it resumes`,
    };
  }
  return { kind: "absent", text: "Not on air in this window" };
}

/** The time extent of the latest interval in the window: `start → live edge` while open (the cap
 * past `t_end_s` is assumption, never measured air — docs/api.md T-410), `start → end` once closed.
 * `null` when the window holds no interval. */
export function extentText(r: Pick<Row, "presence">): string | null {
  const li = r.presence?.last_interval;
  if (!li) return null;
  if (li.open) return `${clockText(li.t_start_s)} → live (measured to ${clockText(li.t_end_s)})`;
  return `${clockText(li.t_start_s)} → ${clockText(li.t_end_s)} · ${spanText(li.t_end_s - li.t_start_s - (li.revoked_s ?? 0))} on air`;
}

/** One line of the "Measured" list. */
export interface MeasuredLine { label: string; value: string; flag?: boolean }

/**
 * The "Measured" block: centre and bandwidth, then the levels **with the time they were measured
 * over** (`measured`, T-350), then the presence totals. The levels are only ever shown from
 * `measured`, so they cannot appear without their time; `at` is the "measured at … over …" line,
 * `null` when nothing has been measured (said in words by the caller, never a zero).
 */
export function measuredBlock(r: DetailRow, edgeS: number | null): { lines: MeasuredLine[]; at: string | null } {
  const { centerHz, bandwidthHz } = detailFreq(r);
  const lines: MeasuredLine[] = [
    { label: "Centre", value: `${(centerHz / 1e6).toFixed(4)} MHz` },
    { label: "Bandwidth", value: bandwidthHz >= 1e6 ? `${(bandwidthHz / 1e6).toFixed(2)} MHz` : `${(bandwidthHz / 1e3).toFixed(1)} kHz` },
  ];
  const m = r.measured ?? null;
  if (m) {
    lines.push({ label: "SNR", value: `${m.snr_db.toFixed(1)} dB` }, { label: "Peak", value: `${m.peak_dbfs.toFixed(1)} dBFS` });
  }
  const ext = extentText(r);
  if (ext) lines.push({ label: "Extent", value: ext });
  const p = r.presence;
  if (p && p.intervals > 0) lines.push({ label: "In window", value: `${p.intervals} event${p.intervals === 1 ? "" : "s"} · ${spanText(p.on_air_s)} on air` });
  if (!m) return { lines, at: null };
  const ago = agoText(m.t_end_s, edgeS);
  return { lines, at: `measured at ${clockText(m.t_start_s)} over ${spanText(m.duration_s)}${ago ? ` · ${ago}` : ""}` };
}

/** The sheet's action row, in the mockup's order, by the context menu's item ids — the sheet
 * reuses `signalMenuItems` so a button calls exactly what the menu item calls (one action set, not
 * two). `label` overrides the menu's wording where the sheet's is the ticket's ("Record clip"). */
export const DETAIL_ACTIONS: readonly { id: string; label?: string; primary?: boolean }[] = [
  { id: "listen", primary: true },
  { id: "decode" },
  { id: "export", label: "Record clip" },
  { id: "stream" },
  { id: "analyze" },
  { id: "promote" },
  { id: "delete" },
];

/** The same row for a selected REGION (T-943). The region panel used to carry no buttons at all —
 * only a sentence telling the viewer to right-click — so Listen and Decode were unreachable from a
 * selection on every surface that shows one. Ids are `selectionMenuItems`'s own, so each button
 * calls exactly what the menu item calls; "Stream out" and "Promote" have no selection meaning and
 * are absent rather than disabled. */
export const SELECTION_DETAIL_ACTIONS: readonly { id: string; label?: string; primary?: boolean }[] = [
  { id: "listen-all", primary: true },
  { id: "decode" },
  { id: "export", label: "Record clip" },
  { id: "analyze" },
  { id: "delete" },
];

/** Picks and orders the sheet's buttons from a menu item list (items the row doesn't offer — e.g.
 * Promote on a confirmed row — are simply absent). */
export function detailActions<T extends { id: string; label: string }>(
  items: readonly T[], order: readonly { id: string; label?: string; primary?: boolean }[] = DETAIL_ACTIONS,
): (T & { primary: boolean })[] {
  const out: (T & { primary: boolean })[] = [];
  for (const a of order) {
    const it = items.find((i) => i.id === a.id);
    // "Stop listening" keeps its own label: it says what the button will do now.
    if (it) out.push({ ...it, label: a.label ?? it.label, primary: a.primary ?? false });
  }
  return out;
}
