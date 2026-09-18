// A minimal PNG decoder, so the browser tier can assert on **pixels the compositor actually
// produced** with no image dependency (T-455).
//
// Why pixels from a screenshot rather than `gl.readPixels`: the surface's context is created
// without `preserveDrawingBuffer`, so a readback after compositing is empty — and more to the
// point, a readback proves the shader ran, which is the claim T-441 already proves on 115 200
// pixels. What this tier has to add is the claim T-441 could not make: that the canvas is in the
// page, the right size, composited, and not covered by a failure card. `Page.captureScreenshot`
// answers exactly that, because it is the frame the user would be looking at.
//
// Scope: 8-bit non-interlaced RGB/RGBA, which is every screenshot Chrome emits. Anything else
// throws rather than guessing.
import { inflateSync } from "node:zlib";

const SIG = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);

/** Decode a PNG buffer to `{ width, height, data }`, `data` being RGBA8 (alpha 255 when absent). */
export function decodePng(buf) {
  if (!buf.subarray(0, 8).equals(SIG)) throw new Error("not a PNG");
  let off = 8, ihdr = null;
  const idat = [];
  while (off + 8 <= buf.length) {
    const len = buf.readUInt32BE(off);
    const type = buf.toString("ascii", off + 4, off + 8);
    const body = buf.subarray(off + 8, off + 8 + len);
    if (type === "IHDR") {
      ihdr = {
        width: body.readUInt32BE(0), height: body.readUInt32BE(4),
        depth: body[8], color: body[9], interlace: body[12],
      };
    } else if (type === "IDAT") idat.push(body);
    else if (type === "IEND") break;
    off += 12 + len;
  }
  if (!ihdr) throw new Error("PNG without IHDR");
  if (ihdr.depth !== 8 || ihdr.interlace !== 0 || (ihdr.color !== 2 && ihdr.color !== 6)) {
    throw new Error(`unsupported PNG: depth=${ihdr.depth} color=${ihdr.color} interlace=${ihdr.interlace}`);
  }
  const ch = ihdr.color === 6 ? 4 : 3;
  const { width, height } = ihdr;
  const raw = inflateSync(Buffer.concat(idat));
  const stride = width * ch;
  const out = Buffer.alloc(width * height * 4, 255);
  let prev = Buffer.alloc(stride);
  for (let y = 0; y < height; y++) {
    const filter = raw[y * (stride + 1)];
    const line = Buffer.from(raw.subarray(y * (stride + 1) + 1, y * (stride + 1) + 1 + stride));
    for (let i = 0; i < stride; i++) {
      const a = i >= ch ? line[i - ch] : 0, b = prev[i], c = i >= ch ? prev[i - ch] : 0;
      let v = line[i];
      if (filter === 1) v += a;
      else if (filter === 2) v += b;
      else if (filter === 3) v += (a + b) >> 1;
      else if (filter === 4) {
        const p = a + b - c, pa = Math.abs(p - a), pb = Math.abs(p - b), pc = Math.abs(p - c);
        v += pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
      } else if (filter !== 0) throw new Error(`bad PNG filter ${filter}`);
      line[i] = v & 0xff;
    }
    for (let x = 0; x < width; x++) {
      const s = x * ch, d = (y * width + x) * 4;
      out[d] = line[s]; out[d + 1] = line[s + 1]; out[d + 2] = line[s + 2];
      if (ch === 4) out[d + 3] = line[s + 3];
    }
    prev = line;
  }
  return { width, height, data: out };
}

/**
 * Summarise a rectangle of an RGBA8 image the way T-441 summarises its shader output: counts, not
 * a golden image. A golden screenshot would fail on every font-rendering difference and be turned
 * off within a week; a histogram states the property the test is actually about.
 */
export function census(img, rect = null) {
  const x0 = rect ? Math.max(0, rect.x) : 0, y0 = rect ? Math.max(0, rect.y) : 0;
  const x1 = rect ? Math.min(img.width, rect.x + rect.w) : img.width;
  const y1 = rect ? Math.min(img.height, rect.y + rect.h) : img.height;
  const colours = new Map();
  let total = 0, sum = 0;
  for (let y = y0; y < y1; y++) {
    for (let x = x0; x < x1; x++) {
      const d = (y * img.width + x) * 4;
      const key = (img.data[d] << 16) | (img.data[d + 1] << 8) | img.data[d + 2];
      colours.set(key, (colours.get(key) ?? 0) + 1);
      sum += img.data[d] + img.data[d + 1] + img.data[d + 2];
      total++;
    }
  }
  const ranked = [...colours].sort((a, b) => b[1] - a[1]);
  return {
    total,
    distinct: colours.size,
    meanLuma: total ? sum / (3 * total) : 0,
    /** Share of the rectangle held by its single commonest colour: 1.0 is a flat fill. */
    dominantShare: total ? ranked[0][1] / total : 0,
    dominant: ranked.length ? `#${ranked[0][0].toString(16).padStart(6, "0")}` : null,
    top: ranked.slice(0, 6).map(([k, n]) => [`#${k.toString(16).padStart(6, "0")}`, n]),
  };
}
