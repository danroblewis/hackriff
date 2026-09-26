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
//
// **T-1003 (MMAP split view): the bars grew into the pane's STATUS CHIP.** The same argument that
// made the bars per pane condemns everything else that described "the" viewport: with two panes
// open, the bottom-left line said where ONE of them was looking (the hidden active pane), the IQ
// sentence said which side of the ring's horizon THAT pane sat on, and the Retune button under
// Go-to commanded a viewport nothing on screen named. The user's words for the result were "the
// info side bar on the bottom left with the Retune button looks out of place" — so the pane's own
// words live inside the pane, as one small translucent chip in the corner its bars already own:
//
//   ──────── 200 kHz          the two bars
//   | 10 s
//   detail · 2 kHz × 0.5 s    the honesty tier and the cells the pixels are made of
//   hackrf-0                  whose coverage decides this pane's grey (T-1006)
//   100.98 MHz ± 937 kHz · LIVE     where THIS pane is looking
//   raw IQ in the ring here   what backs THIS pane's own time position (the playback horizon)
//   slice … peak −62 dB       its trace readout, while its trace layer is on
//   [Retune] 2 MHz at 101.3   its own offer, pressed for THIS pane
//
// Every line is optional and every line is that pane's: a chip states what its pane has to say and
// nothing else, so pressing or freezing pane 1 cannot change a word inside pane 2.

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
  /** T-1006: whose coverage decides this pane's grey, or `null` when the host names none. */
  readonly device: ScaleDevice | null;
  /** T-1003: where THIS pane is looking — centre ± span · LIVE, or how far behind the edge. */
  readonly where: string;
  /** T-1003: what raw IQ backs THIS pane's own time position, in words (`null`: nothing to say). */
  readonly iq: string | null;
  /** T-1003: this pane's trace readout, while its trace layer is on; `null` when it is off. */
  readonly trace: string | null;
  /** T-1003: this pane's own Retune offer, `null` when the host offers none for it. */
  readonly retune: ScaleAction | null;
  /** T-1003: one sentence about a retune happening to THIS pane now (T-1028's status line). */
  readonly retuneStatus: string | null;
  /** The widest the chip may be drawn, in CSS px — the pane's own width less its insets. */
  readonly maxWidthPx: number;
  readonly state: Readonly<Record<string, string>>;
}

/**
 * T-1003: a pressable offer on the chip — the same strings-and-a-bit shape as `chrome.ts`'s
 * `RowAction`, for the same reason: this file never learns what a retune is, it paints the words it
 * is handed and reports the press back to the host with the pane id on it.
 */
export interface ScaleAction {
  readonly label: string;
  readonly why: string;
  readonly enabled: boolean;
}

/**
 * T-1006's device pill, carried to the scale block (T-996 retired the per-viewport row it was on).
 * The same strings-and-a-bit shape as `chrome.ts`'s `RowDevice`: this file never interprets
 * `device`, it states the host's words and marks the selector on the element.
 */
export interface ScaleDevice {
  readonly device: string;
  readonly label: string;
  readonly why: string;
  readonly stale: boolean;
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
  /** T-1006: the pane's device pill (optional: a host that names no front end passes none). */
  readonly device?: ScaleDevice | null;
  /** T-1003: this pane's IQ backing in words, and the backing itself for the dataset. */
  readonly iq?: string | null;
  readonly backing?: string;
  /** T-1003: this pane's trace readout while its layer is on. */
  readonly trace?: string | null;
  /** T-1003: this pane's own Retune offer and retune-mode sentence. */
  readonly retune?: ScaleAction | null;
  readonly retuneStatus?: string | null;
}

const NS_PER_S = 1e9;

/** CSS px the chip is inset from its pane's right edge and left free of, together (`.sf-scale`). */
export const CHIP_INSET_PX = 72;
/**
 * The widest a chip is ever drawn, however wide its pane is.
 *
 * It is a corner legend, not a panel: the user asked for "ONE small translucent chip next to the
 * scale bar", and a chip allowed to grow with the pane would be a 1.2 kpx band across the bottom of
 * a 1280 px window — the "info side bar" complaint again, moved to the other corner. Lines wrap
 * inside this; a narrow pane bounds it further still.
 */
export const CHIP_MAX_W_PX = 280;

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
  // T-1003: the kept centre/span/LIVE readout, for THIS pane — the two strings the frame's own
  // status already formatted, joined here rather than by a second reader of the pane's window.
  const where = [report.freqLabel, report.timeLabel].filter(Boolean).join(" · ");
  return {
    paneId: pane.id,
    right,
    bottom,
    scale,
    tier: report.tier,
    level,
    title: report.tierLabel,
    device: report.device ?? null,
    where,
    iq: report.iq ?? null,
    trace: report.trace ?? null,
    retune: report.retune ?? null,
    retuneStatus: report.retuneStatus ?? null,
    // The chip is the pane's, so it is bounded by the pane: a line that ran past the pane's own
    // left edge would be describing a neighbour's picture. The insets match `.sf-scale`'s CSS.
    maxWidthPx: Math.max(96, Math.min(CHIP_MAX_W_PX, wPx - CHIP_INSET_PX)),
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
      // T-1006: the selector as STATE (`data-device`, `data-device-stale`), as the retired row
      // marked it — a `device_id` is exactly the kind of string a sentence mangles.
      device: report.device?.device ?? "",
      deviceStale: report.device?.stale ? "true" : "false",
      // T-1003: what THIS pane's time position is backed by, as the `.sf-ring` dataset says it for
      // the active one — a word a test compares against the pane's own `t1Ns` above, never a
      // sentence it has to parse.
      backing: report.backing ?? "",
    },
  };
}

/**
 * The band-2 scale layer: one block per pane, bottom-right, pooled and re-laid-out **inside the
 * render frame** like the HUD labels. Never on a poll — a scale bar quoting last second's zoom
 * beside this frame's pixels is the same defect as a box drifting from its rows.
 */
export class ScaleBars {
  private readonly pool: Chip[] = [];

  /**
   * `host` is how a press leaves this file: the chip paints the words it was handed and reports the
   * press back **with the pane id on it**, so a Retune inside pane 1 can only ever name pane 1. A
   * layer built without a host paints the same chip with its button disabled — the developer
   * preview has no device path to offer.
   */
  constructor(private readonly root: HTMLElement, private readonly host?: ScaleHost) {}

  update(marks: readonly ScaleMark[]): void {
    const doc = this.root.ownerDocument;
    while (this.pool.length < marks.length) this.pool.push(this.mint(doc));
    for (let i = 0; i < this.pool.length; i++) {
      const c = this.pool[i];
      const m = marks[i];
      if (!m) { if (!c.el.hidden) c.el.hidden = true; continue; }
      if (c.el.hidden) c.el.hidden = false;
      // Anchored at the pane's bottom-right corner, inset by the block's own margin (CSS).
      c.el.style.transform = `translate(${m.right.toFixed(1)}px, ${m.bottom.toFixed(1)}px)`;
      // T-1003: and bounded by the pane it belongs to, so a long sentence wraps inside its own
      // rectangle instead of running across the split into the neighbouring picture.
      const maxW = `${Math.round(m.maxWidthPx)}px`;
      if (c.el.style.maxWidth !== maxW) c.el.style.maxWidth = maxW;
      bar(c.freq, m.scale.freq, "width");
      bar(c.time, m.scale.time, "height");
      setText(c.level, m.level);
      if (c.device.hidden !== !m.device) c.device.hidden = !m.device;
      if (m.device) {
        setText(c.device, m.device.label);
        if (c.device.title !== m.device.why) c.device.title = m.device.why;
      }
      // T-1003's per-pane words. Each is hidden when its pane has nothing to say rather than left
      // showing the last frame's sentence: an empty line claims nothing, a stale one claims wrong.
      say(c.where, m.where);
      say(c.iq, m.iq);
      say(c.trace, m.trace);
      say(c.retuneStatus, m.retuneStatus);
      if (c.retune.hidden !== !m.retune) c.retune.hidden = !m.retune;
      if (m.retune) {
        setText(c.retuneGo, m.retune.label);
        if (c.retuneGo.title !== m.retune.why) c.retuneGo.title = m.retune.why;
        c.retuneGo.disabled = !m.retune.enabled || !this.host;
        c.retuneGo.setAttribute("aria-disabled", m.retune.enabled && this.host ? "false" : "true");
        say(c.retuneWhy, m.retune.why);
      }
      if (c.el.title !== m.title) c.el.title = m.title;
      for (const [k, v] of Object.entries(m.state)) if (c.el.dataset[k] !== v) c.el.dataset[k] = v;
    }
  }

  dispose(): void {
    for (const c of this.pool) c.el.remove();
    this.pool.length = 0;
  }

  private mint(doc: Document): Chip {
    const el = doc.createElement("div");
    el.className = "sf-scale";
    const rows: HTMLElement[] = [];
    for (const axis of ["freq", "time"]) {
      const row = doc.createElement("div");
      row.className = `sf-scale-row ${axis}`;
      row.append(doc.createElement("i"), doc.createElement("b"));
      rows.push(row);
      el.append(row);
    }
    const level = span(doc, "sf-scale-level");
    // T-1006: the device pill, hidden until the host names a front end.
    const device = span(doc, "sf-scale-device");
    device.hidden = true;
    // T-1003. `.sf-where` keeps its name: it is the same centre/span/LIVE readout T-996 kept, moved
    // out of the one bottom-left line into the pane it describes.
    const where = span(doc, "sf-where");
    const iq = span(doc, "sf-pane-iq");
    const trace = span(doc, "sf-pane-trace");
    const retuneGo = doc.createElement("button");
    retuneGo.type = "button";
    retuneGo.className = "sf-pane-retune-go";
    // The press names the pane the chip is CURRENTLY placed on, read off the element the frame just
    // wrote — never a pane id closed over at mint time, because the pool is indexed by position and
    // a chip outlives the pane it was first minted for (the same rule the width buttons keep).
    retuneGo.addEventListener("click", () => {
      const id = el.dataset.pane;
      if (id && !retuneGo.disabled) this.host?.pressRetune(id);
    });
    const retuneWhy = span(doc, "sf-pane-retune-why");
    const retune = doc.createElement("div");
    retune.className = "sf-pane-retune";
    retune.hidden = true;
    retune.append(retuneGo, retuneWhy);
    // T-1028's sentence, for THIS pane: `role="status"` so a retune the user's own pan asked for is
    // heard, not only seen.
    const retuneStatus = span(doc, "sf-pane-retune-status");
    retuneStatus.setAttribute("role", "status");
    el.append(level, device, where, iq, trace, retune, retuneStatus);
    this.root.append(el);
    return { el, freq: rows[0], time: rows[1], level, device, where, iq, trace, retune, retuneGo, retuneWhy, retuneStatus };
  }
}

/** One pooled chip's elements, named once at mint — a chip is read by name, never by child index. */
interface Chip {
  readonly el: HTMLElement;
  readonly freq: HTMLElement;
  readonly time: HTMLElement;
  readonly level: HTMLElement;
  readonly device: HTMLElement;
  readonly where: HTMLElement;
  readonly iq: HTMLElement;
  readonly trace: HTMLElement;
  readonly retune: HTMLElement;
  readonly retuneGo: HTMLButtonElement;
  readonly retuneWhy: HTMLElement;
  readonly retuneStatus: HTMLElement;
}

/** What a chip asks of its host: exactly one press, and it names the pane it came from. */
export interface ScaleHost {
  pressRetune(paneId: string): void;
}

function span(doc: Document, cls: string): HTMLElement {
  const el = doc.createElement("span");
  el.className = cls;
  return el;
}

function setText(el: HTMLElement, t: string): void {
  if (el.textContent !== t) el.textContent = t;
}

/** A line with nothing to say is emptied AND hidden — never left holding the last frame's words. */
function say(el: HTMLElement, t: string | null): void {
  const on = !!t;
  if (el.hidden === on) el.hidden = !on;
  setText(el, t ?? "");
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
