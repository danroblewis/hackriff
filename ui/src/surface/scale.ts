// T-996 (MMAP): the **scale bar** — a map's answer to "how big is what I am looking at?".
//
// The user's nit-pick (2026-09-25, via the supervisor) retired the white panel that stated a pane's
// centre and span as a sentence ("100.980 MHz ± 937 kHz · LIVE") and the `More` expansion of
// paragraphs beside it. What a map gives you instead is the thing in Google Maps' bottom-right: a
// short bar with the distance it spans. Two of them here, because this surface has two axes:
//
//   ──────── 200 kHz      the FREQUENCY the bar's length spans, at this pane's zoom
//   |                     the TIME its height spans
//   | 10 s
//
// The bar is the reading, not the number: its LENGTH is what carries the scale, so the number beside
// it is a round one (1, 2 or 5 × a power of ten) chosen to make the bar come out under a budget of
// pixels, exactly as a map's does. That is the whole of the arithmetic here, and it is pure: the
// caller passes the pane's own Hz-per-pixel and seconds-per-pixel — derived, per render frame, from
// the SAME `box`/`rect` the renderer drew that pane with — and gets back a length in CSS px and the
// words for it. Nothing in this file knows about the DOM, a device, or a route.
//
// **Why per pane and per frame.** A split has two panes at two zooms; a scale drawn from the "the"
// view would be false on at least one of them. And a pane's Hz/px changes on every wheel and every
// drag, so a bar computed on a poll would state last second's zoom against this frame's pixels —
// the T-388 family (two derivations of one picture), which the canvas invariants forbid outright.

/** One bar: how long to draw it, what quantity that length spans, and how to say the quantity. */
export interface ScaleBar {
  /** The bar's length, in the same pixel unit as the `maxPx` budget it was chosen under (CSS px). */
  readonly px: number;
  /** The quantity that length spans — Hz for the frequency bar, seconds for the time bar. */
  readonly value: number;
  /** `value`, in words: "200 kHz", "10 s". */
  readonly label: string;
}

/** Both of a pane's bars. Either may be `null` when the pane is too degenerate to state one. */
export interface PaneScale {
  readonly freq: ScaleBar | null;
  readonly time: ScaleBar | null;
}

/** The round frequencies a bar may stand for: 1 / 2 / 5 per decade, 1 Hz … 1 GHz. */
const HZ_STEPS: readonly number[] = decades(1, 1e9);
/**
 * The round durations a bar may stand for.
 *
 * 1/2/5 per decade below a second (a bar is a bar, and 500 ms is as round as 500 kHz), then the
 * ladder a clock actually uses: nobody reads "100 s", they read "2 min". This is the one place the
 * two axes differ, and they differ because seconds are not decimal above 1.
 */
const S_STEPS: readonly number[] = [
  ...decades(1e-3, 0.5),
  1, 2, 5, 10, 15, 30,
  60, 120, 300, 600, 900, 1800,
  3600, 7200, 10800, 21600, 43200, 86400,
];

function decades(lo: number, hi: number): number[] {
  const out: number[] = [];
  for (let e = Math.round(Math.log10(lo)); Math.pow(10, e) <= hi * 1.0000001; e++) {
    for (const m of [1, 2, 5]) {
      const v = m * Math.pow(10, e);
      if (v >= lo * 0.9999999 && v <= hi * 1.0000001) out.push(v);
    }
  }
  return out;
}

/**
 * The longest step in `steps` that fits inside `maxPx` at `perPx` units per pixel.
 *
 * `null` when the view is degenerate (a zero or non-finite scale) or when even the smallest step on
 * the ladder would overflow the budget — a bar that claimed a quantity it is not the length of would
 * be worse than no bar, and this surface's rule is that a thing not drawn claims nothing.
 */
export function niceBar(perPx: number, maxPx: number, steps: readonly number[], label: (v: number) => string): ScaleBar | null {
  if (!(perPx > 0) || !Number.isFinite(perPx) || !(maxPx > 0)) return null;
  const budget = perPx * maxPx;
  let best: number | null = null;
  for (const v of steps) {
    // A hair of slack: `budget` is a float quotient, and a step that is exactly the budget (a pane
    // sized to a round span, which is common — 20 MHz across 400 px) must not be rejected by 1 ulp.
    if (v <= budget * (1 + 1e-9)) best = v;
    else break;
  }
  if (best === null) return null;
  return { px: best / perPx, value: best, label: label(best) };
}

/** A round frequency, in the largest unit that leaves it a whole number: "200 kHz", "1 MHz". */
export function fmtScaleHz(hz: number): string {
  const [v, u] = hz >= 1e9 ? [hz / 1e9, "GHz"] : hz >= 1e6 ? [hz / 1e6, "MHz"] : hz >= 1e3 ? [hz / 1e3, "kHz"] : [hz, "Hz"];
  return `${trim(v)} ${u}`;
}

/** A round duration, in the unit a clock would use: "500 ms", "10 s", "2 min", "6 h". */
export function fmtScaleS(s: number): string {
  const [v, u] = s >= 86400 ? [s / 86400, "d"] : s >= 3600 ? [s / 3600, "h"] : s >= 60 ? [s / 60, "min"] : s >= 1 ? [s, "s"] : [s * 1000, "ms"];
  return `${trim(v)} ${u}`;
}

/** Round to at most 2 dp and drop a trailing `.0` — the steps are round, so this only tidies. */
function trim(v: number): string {
  return String(Math.round(v * 100) / 100);
}

/** The frequency bar for a pane showing `spanHz` across `widthPx` CSS pixels. */
export function freqScaleBar(spanHz: number, widthPx: number, maxPx: number): ScaleBar | null {
  if (!(widthPx > 0)) return null;
  return niceBar(spanHz / widthPx, maxPx, HZ_STEPS, fmtScaleHz);
}

/** The time bar for a pane showing `spanS` down `heightPx` CSS pixels. */
export function timeScaleBar(spanS: number, heightPx: number, maxPx: number): ScaleBar | null {
  if (!(heightPx > 0)) return null;
  return niceBar(spanS / heightPx, maxPx, S_STEPS, fmtScaleS);
}

/**
 * Both bars for one pane, from the box and rectangle the frame drew it with.
 *
 * `widthPx`/`heightPx` are the pane's own size in CSS px; `maxPx` is the budget each bar is chosen
 * under — a Google-Maps-sized bar, and the reason the whole thing stays small at a 400 px width.
 */
export function paneScale(
  spanHz: number, spanS: number, widthPx: number, heightPx: number, maxPx: number,
): PaneScale {
  return { freq: freqScaleBar(spanHz, widthPx, maxPx), time: timeScaleBar(spanS, heightPx, maxPx) };
}

/**
 * The scale bar's budget in CSS px for a pane `widthPx` wide.
 *
 * A quarter of the pane, capped at 96 px and floored at 32: the bar is a legend, not a ruler across
 * the picture, and at a 400 px phone width a fixed 96 px bar plus its label would run into the
 * right-edge control cluster.
 */
export function scaleBudgetPx(widthPx: number): number {
  return Math.max(32, Math.min(96, Math.round(widthPx / 4)));
}

// ---------------------------------------------------------------------------
// Placement and the band-2 DOM layer
// ---------------------------------------------------------------------------

/**
 * One pane's scale block for one frame: where it goes, what it says, and the frame's own report of
 * what the pane was drawn at.
 *
 * `state` is the dataset written onto the element — the numbers the bars were computed FROM, and the
 * level/tier/counts the frame reported. Same discipline as `.sf-ring`'s dataset (T-506): a sentence
 * is for a reader, the dataset is what a test checks the PIXELS against, and both come off the one
 * frame so they cannot disagree.
 */
export interface ScaleMark {
  readonly paneId: string;
  /** CSS px from the canvas's top-left: the pane's right edge and its bottom edge. */
  readonly right: number;
  readonly bottom: number;
  readonly scale: PaneScale;
  /** The honesty tier this pane was drawn from (`detail` / `overview`) — the three-tier rule. */
  readonly tier: string;
  /** The tiny line beside the bars: the tier and the cell size the pixels are made of. */
  readonly level: string;
  /** That statement in full, as the block's tooltip. */
  readonly title: string;
  readonly state: Readonly<Record<string, string>>;
}

/** The pane geometry a mark is placed from — the `box` and `rect` the frame drew the pane with. */
export interface ScaleSource {
  readonly id: string;
  readonly f0Hz: number;
  readonly f1Hz: number;
  readonly t0Ns: number;
  readonly t1Ns: number;
  /** Device px, GL convention (origin bottom-left), exactly as `PaneView.rect` gives it. */
  readonly rect: { readonly x: number; readonly y: number; readonly w: number; readonly h: number };
}

/** What the frame reported about the pane, in the words `paneStatuses` already formatted. */
export interface ScaleReport {
  readonly tier: string;
  /** Whether this viewport is following the live edge, asked of the model that owns the answer. */
  readonly following: boolean;
  readonly tierLabel: string;
  readonly levelLabel: string;
  readonly freqLabel: string;
  readonly timeLabel: string;
  readonly counts: string;
}

const NS_PER_S = 1e9;

/**
 * A pane's scale block, in CSS px from the canvas's top-left.
 *
 * `canvasHpx` is the drawing buffer's height (device px): the GL rect's origin is bottom-left and
 * the DOM's is top-left, the same conversion `hudLabels` does. Everything here comes from the ONE
 * frame's box and rect, which is why the bar cannot drift out of step with the rows it measures.
 */
export function scaleMarkOf(
  pane: ScaleSource, report: ScaleReport, canvasHpx: number, dpr: number,
): ScaleMark | null {
  const k = dpr > 0 ? dpr : 1;
  const wPx = pane.rect.w / k, hPx = pane.rect.h / k;
  if (!(wPx > 0) || !(hPx > 0)) return null;
  const right = (pane.rect.x + pane.rect.w) / k;
  const bottom = (canvasHpx - pane.rect.y) / k;
  const spanHz = pane.f1Hz - pane.f0Hz, spanS = (pane.t1Ns - pane.t0Ns) / NS_PER_S;
  const scale = paneScale(spanHz, spanS, wPx, hPx, scaleBudgetPx(Math.min(wPx, hPx)));
  // The tier, then the cells: "detail · 2.00 kHz × 0.5 s cells". `levelLabel` says the same thing
  // with the level indices, and is the tooltip — the tiny line is the part a glance needs.
  const level = `${report.tier} · ${report.levelLabel.replace(/\s*\(.*$/, "")}`;
  return {
    paneId: pane.id,
    right,
    bottom,
    scale,
    tier: report.tier,
    level,
    title: report.tierLabel,
    state: {
      pane: pane.id,
      following: report.following ? "true" : "false",
      hzPerPx: String(spanHz / wPx),
      sPerPx: String(spanS / hPx),
      paneWPx: String(wPx),
      paneHPx: String(hPx),
      spanHz: String(spanHz),
      spanS: String(spanS),
      // The window the bars were laid out against, unrounded — absolute capture ns, the surface's
      // one time vocabulary. `data-t0-ns` / `data-t1-ns`, as the retired viewport row carried them:
      // a test reads STATE, not a rounded sentence (T-472).
      t0Ns: String(pane.t0Ns),
      t1Ns: String(pane.t1Ns),
      fPx: scale.freq ? String(scale.freq.px) : "",
      fHz: scale.freq ? String(scale.freq.value) : "",
      tPx: scale.time ? String(scale.time.px) : "",
      tS: scale.time ? String(scale.time.value) : "",
      tier: report.tier,
      level: report.levelLabel,
      where: report.freqLabel,
      when: report.timeLabel,
      counts: report.counts,
    },
  };
}

/**
 * The band-2 scale layer: one block per pane, bottom-right, pooled and re-laid-out **inside the
 * render frame** like the HUD labels. Never on a poll — a scale bar quoting last second's zoom
 * beside this frame's pixels is the same defect as a box drifting from its rows.
 */
export class ScaleBars {
  private readonly pool: HTMLElement[] = [];

  constructor(private readonly root: HTMLElement) {}

  update(marks: readonly ScaleMark[]): void {
    const doc = this.root.ownerDocument;
    while (this.pool.length < marks.length) this.pool.push(this.mint(doc));
    for (let i = 0; i < this.pool.length; i++) {
      const el = this.pool[i];
      const m = marks[i];
      if (!m) { if (!el.hidden) el.hidden = true; continue; }
      if (el.hidden) el.hidden = false;
      // Anchored at the pane's bottom-right corner, inset by the block's own margin (CSS).
      el.style.transform = `translate(${m.right.toFixed(1)}px, ${m.bottom.toFixed(1)}px)`;
      const [fRow, tRow, lvl] = [el.children[0] as HTMLElement, el.children[1] as HTMLElement, el.children[2] as HTMLElement];
      bar(fRow, m.scale.freq, "width");
      bar(tRow, m.scale.time, "height");
      if (lvl.textContent !== m.level) lvl.textContent = m.level;
      if (el.title !== m.title) el.title = m.title;
      for (const [k, v] of Object.entries(m.state)) if (el.dataset[k] !== v) el.dataset[k] = v;
    }
  }

  dispose(): void {
    for (const el of this.pool) el.remove();
    this.pool.length = 0;
  }

  private mint(doc: Document): HTMLElement {
    const el = doc.createElement("div");
    el.className = "sf-scale";
    for (const axis of ["freq", "time"]) {
      const row = doc.createElement("div");
      row.className = `sf-scale-row ${axis}`;
      const line = doc.createElement("i");
      const label = doc.createElement("b");
      row.append(line, label);
      el.append(row);
    }
    const lvl = doc.createElement("span");
    lvl.className = "sf-scale-level";
    el.append(lvl);
    this.root.append(el);
    return el;
  }
}

/** Draw one bar row: the line's length along its axis, and the quantity beside it. */
function bar(row: HTMLElement, b: ScaleBar | null, axis: "width" | "height"): void {
  // A bar with nothing honest to say is hidden, not drawn at a token length: the length IS the claim.
  if (!b) { if (!row.hidden) row.hidden = true; return; }
  if (row.hidden) row.hidden = false;
  const line = row.children[0] as HTMLElement, label = row.children[1] as HTMLElement;
  const px = `${b.px.toFixed(1)}px`;
  if (line.style[axis] !== px) line.style[axis] = px;
  if (label.textContent !== b.label) label.textContent = b.label;
}
