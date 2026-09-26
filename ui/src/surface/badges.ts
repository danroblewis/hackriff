// T-994: the output-kind vocabulary of a feature's ACTIVE badge — the small corner badge a box
// carries on the map while an output is open on it (a Listen stream, a streamed pipeline output, a
// decode pipeline, a recording). Presentation only: which outputs are open is the backend's answer
// (`app/dock/activity.ts` reads it), this file only names and draws the kinds.

export type OutputKind = "audio" | "stream" | "decode" | "rec";

/** Badge order. */
export const OUTPUT_KINDS: readonly OutputKind[] = ["audio", "stream", "decode", "rec"];
/** The words each glyph stands for (accessible name, title). */
export const OUTPUT_KIND_WORDS: Readonly<Record<OutputKind, string>> = {
  audio: "listening", stream: "streaming out", decode: "decoding", rec: "recording",
};
/** The glyphs: each a distinct character, so a kind is never told by hue alone. */
export const OUTPUT_KIND_GLYPH: Readonly<Record<OutputKind, string>> = {
  audio: "♪", stream: "⇢", decode: "01", rec: "●",
};

/** One feature's open outputs as the map draws them. */
export interface FeatureActivity {
  /** In [[OUTPUT_KINDS]] order, without repeats. */
  readonly kinds: readonly OutputKind[];
  /** Live audio level, 0..1 (the server-reported level), for the pulse; null = none. */
  readonly level: number | null;
}

/** "listening, decoding" — for an accessible name. */
export function activityWords(a: FeatureActivity | undefined | null): string {
  return a ? a.kinds.map((k) => OUTPUT_KIND_WORDS[k]).join(", ") : "";
}
