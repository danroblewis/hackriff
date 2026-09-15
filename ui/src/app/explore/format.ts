// Pure formatting helpers for the Explore panels (ADR-0013 §1 thin-client rule): every string here
// formats fields the API already serves; nothing here computes a new signal fact. Unit-tested
// without a DOM in ui/test/app-explore-format.test.ts.
import type { ExplanationEvidence, RefinedTuning } from "./inventory";

/** Frequency in MHz, fixed decimals (default matches the focus panel's big frequency). */
export function fmtMHz(hz: number, decimals = 4): string {
  return (hz / 1e6).toFixed(decimals);
}

/** Bandwidth as kHz below 1 MHz, else MHz — same rounding rule everywhere it's shown. */
export function fmtBandwidth(hz: number): string {
  if (hz >= 1e6) return `${(hz / 1e6).toFixed(hz >= 10e6 ? 1 : 2)} MHz`;
  return `${Math.round(hz / 1e3)} kHz`;
}

export function fmtPct(x: number): string {
  return `${Math.round(x * 100)}%`;
}

/** The server's own `{error, code}` body, or a plain message; used for action failures. Reads
 * `message` off a plain thrown object too (`ControlError` and a bare `{code, message}` both work),
 * falling back to `String(e)` only when neither is present. */
export function apiErrorText(e: unknown): string {
  const anyE = e as { code?: unknown; message?: unknown };
  const code = anyE && typeof anyE.code === "string" ? anyE.code : null;
  const msg = anyE && typeof anyE.message === "string" ? anyE.message : e instanceof Error ? e.message : String(e);
  return code ? `${msg} (${code})` : msg;
}

/** One evidence item's contribution to an explanation's "why" line — every clause is a field the
 * API already served (a band-plan reason, a raster offset, a classifier confidence), joined, never
 * invented. */
function evidenceWhy(e: ExplanationEvidence): string | null {
  switch (e.kind) {
    case "band-plan":
      return e.reason;
    case "raster":
      return e.on_raster
        ? `on the ${fmtBandwidth(e.raster_hz)} raster`
        : `${fmtBandwidth(Math.abs(e.offset_hz))} off the ${fmtBandwidth(e.raster_hz)} raster`;
    case "family":
      return `classifier confidence ${fmtPct(e.confidence)}`;
    default:
      return null;
  }
}

/** Formats an explanation's evidence into one "why" sentence (§4.5 "ranked suggestions"). */
export function explanationWhy(evidence: readonly ExplanationEvidence[]): string {
  return evidence.map(evidenceWhy).filter((s): s is string => !!s).join("; ");
}

/** The focus panel's "Channel raster" row: "on raster" / "N kHz off raster" / "—" without raster
 * evidence (a service with no raster table, or nothing ranked yet). */
export function rasterText(evidence: readonly ExplanationEvidence[]): string {
  const r = evidence.find((e): e is Extract<ExplanationEvidence, { kind: "raster" }> => e.kind === "raster");
  if (!r) return "—";
  return r.on_raster ? "on raster" : `${fmtBandwidth(Math.abs(r.offset_hz))} off raster`;
}

/** The focus panel's refined-from-output note (§4.5): "Centre from detection; not refined yet"
 * until an [[RefinedTuning]] exists, else a plain fact about it (mode, convergence). */
export function refinedNote(refined: RefinedTuning | null): string {
  if (!refined) return "Centre from detection; not refined yet";
  return `Refined from the ${refined.mode} output${refined.converged ? "" : " (not yet converged)"}`;
}
