// Small SVG plot primitives for the Decode workbench (ADR-0013 §8, T-153). Pure geometry over
// numbers the caller already computed from served data — no signal logic. Any served string (a
// node id, a metadata value) is set with `.textContent`, never interpolated into markup.
const NS = "http://www.w3.org/2000/svg";

function el<K extends keyof SVGElementTagNameMap>(tag: K, attrs: Record<string, string | number> = {}): SVGElementTagNameMap[K] {
  const e = document.createElementNS(NS, tag) as SVGElementTagNameMap[K];
  for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, String(v));
  return e;
}

function text(x: number, y: number, s: string, opts: { anchor?: "start" | "middle" | "end"; fill?: string; size?: number } = {}): SVGTextElement {
  const t = el("text", { x, y, "text-anchor": opts.anchor ?? "start", fill: opts.fill ?? "var(--dim)", "font-size": opts.size ?? 10.5, "font-family": "var(--mono)" });
  t.textContent = s;
  return t;
}

function root(w: number, h: number): SVGSVGElement {
  const s = el("svg", { viewBox: `0 0 ${w} ${h}`, width: "100%", height: "100%", preserveAspectRatio: "none" });
  s.setAttribute("role", "img");
  return s;
}

/** An honest "not available" plot: a border and one or more caption lines, never fabricated data. */
export function placeholderPlot(w: number, h: number, lines: readonly string[]): SVGSVGElement {
  const s = root(w, h);
  s.append(el("rect", { x: 1, y: 1, width: w - 2, height: h - 2, fill: "none", stroke: "var(--line-soft)", "stroke-dasharray": "4 4", rx: 6 }));
  const n = lines.length;
  lines.forEach((l, i) => s.append(text(w / 2, h / 2 - ((n - 1) * 14) / 2 + i * 14, l, { anchor: "middle", fill: "var(--dim)" })));
  return s;
}

/** A time-series line plot of already-computed y values, scaled to fit. */
export function linePlot(w: number, h: number, values: readonly number[], opts: { stroke?: string; unit?: string } = {}): SVGSVGElement {
  const s = root(w, h);
  const L = 8, R = 6, T = 8, B = 18;
  if (values.length < 2) return placeholderPlot(w, h, ["no samples yet"]);
  let lo = Infinity, hi = -Infinity;
  for (const v of values) { if (v < lo) lo = v; if (v > hi) hi = v; }
  if (!Number.isFinite(lo) || !Number.isFinite(hi)) return placeholderPlot(w, h, ["no samples yet"]);
  if (hi - lo < 1e-9) { hi += 1; lo -= 1; }
  const x = (i: number) => L + (i / (values.length - 1)) * (w - L - R);
  const y = (v: number) => T + (1 - (v - lo) / (hi - lo)) * (h - T - B);
  let d = "";
  values.forEach((v, i) => { d += `${i ? "L" : "M"}${x(i).toFixed(1)},${y(v).toFixed(1)} `; });
  s.append(el("line", { x1: L, x2: w - R, y1: h - B, y2: h - B, stroke: "var(--line-soft)" }));
  s.append(el("path", { d, fill: "none", stroke: opts.stroke ?? "var(--teal)", "stroke-width": 1.3 }));
  s.append(text(L, h - 4, `${values.length} samples${opts.unit ? ` · ${opts.unit}` : ""}`));
  return s;
}

/** A scatter plot of already-computed points (e.g. raw I/Q samples), decimated by the caller. */
export function scatterPlot(w: number, h: number, points: readonly { x: number; y: number }[], opts: { color?: string } = {}): SVGSVGElement {
  const s = root(w, h);
  if (points.length === 0) return placeholderPlot(w, h, ["no samples yet"]);
  let m = 0;
  for (const p of points) m = Math.max(m, Math.abs(p.x), Math.abs(p.y));
  if (m < 1e-9) m = 1;
  const cx = w / 2, cy = h / 2, sc = (Math.min(w, h) / 2 - 12) / m;
  s.append(el("line", { x1: 8, x2: w - 8, y1: cy, y2: cy, stroke: "var(--line-soft)" }));
  s.append(el("line", { x1: cx, x2: cx, y1: 8, y2: h - 8, stroke: "var(--line-soft)" }));
  for (const p of points) {
    s.append(el("circle", { cx: (cx + p.x * sc).toFixed(1), cy: (cy - p.y * sc).toFixed(1), r: 1.4, fill: opts.color ?? "var(--teal)", "fill-opacity": 0.55 }));
  }
  return s;
}

/** A strip of hard 0/1 values (bits). */
export function bitStripPlot(w: number, h: number, bits: readonly number[]): SVGSVGElement {
  const s = root(w, h);
  if (bits.length === 0) return placeholderPlot(w, h, ["no samples yet"]);
  const L = 4, R = 4, T = 20, B = 20, n = Math.min(bits.length, 256);
  const bw = (w - L - R) / n;
  for (let i = 0; i < n; i++) {
    const on = bits[i] > 0;
    s.append(el("rect", { x: L + i * bw, y: on ? T : h - B, width: Math.max(1, bw - 0.5), height: h - T - B, fill: on ? "var(--teal)" : "var(--line-soft)", "fill-opacity": on ? 0.8 : 1 }));
  }
  s.append(text(L, h - 4, `${bits.length} bits shown (first ${n})`));
  return s;
}

/** A horizontal tally chart: `{label, count}` bars, longest first. */
export function tallyPlot(w: number, h: number, data: readonly { label: string; count: number }[]): SVGSVGElement {
  const s = root(w, h);
  if (data.length === 0) return placeholderPlot(w, h, ["nothing tallied yet"]);
  const L = 60, R = 40, max = Math.max(1, ...data.map((d) => d.count));
  const bh = Math.min(20, (h - 24) / data.length - 6);
  data.forEach((d, i) => {
    const y = 10 + i * (bh + 6);
    const bw = ((w - L - R) * d.count) / max;
    s.append(text(L - 6, y + bh * 0.75, d.label, { anchor: "end", fill: "var(--amber)" }));
    s.append(el("rect", { x: L, y, width: Math.max(1, bw), height: bh, rx: 2, fill: "var(--teal)", "fill-opacity": 0.7 }));
    s.append(text(L + bw + 6, y + bh * 0.75, String(d.count), { fill: "var(--muted)" }));
  });
  return s;
}
