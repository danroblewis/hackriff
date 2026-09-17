// T-437 spike — the SHARED tile-texture LRU.
//
// docs/16 §8.3: "keyed by (level_f, level_t, f_block, t_block) ... A tile visible in two
// panes uploads ONCE - and that sharing is precisely why this must be ONE context rather
// than one per pane."
//
// This file is the load-bearing claim of item 2, so it counts everything: uploads, hits,
// misses, evictions, bytes resident, in-flight. `uploads` is the number that has to stay
// equal to `distinctTiles` no matter how many panes show the same tile.

import { tileKey } from "./view.js";

export const TILE_BYTES = 256 * 256 * 3; // RGB8: R = value, G = coverage duty, B = tier

export class TileCache {
  /**
   * @param gl WebGL2 context (ONE, shared by every pane)
   * @param source async (tile) -> { rgb: Uint8Array(256*256*3), tier, partial } | null
   * @param budgetTiles LRU capacity in tiles (§5.2's "client-side tile eviction")
   * @param inflightCap §5.2's "tiles-in-flight cap" for bounded browser memory
   */
  constructor(gl, source, { budgetTiles = 192, inflightCap = 12 } = {}) {
    this.gl = gl;
    this.source = source;
    this.budgetTiles = budgetTiles;
    this.inflightCap = inflightCap;
    this.map = new Map();      // key -> { tex, lastUsed, pinned, key }
    this.inflight = new Map(); // key -> Promise
    this.queue = [];           // pending tile descriptors, most-wanted last
    this.clock = 0;
    this.stats = {
      uploads: 0, hits: 0, misses: 0, evictions: 0, requests: 0,
      fetches: 0, fetchBytes: 0, uploadMs: 0, refetchAfterEvict: 0,
    };
    this.evicted = new Set(); // keys we have evicted at least once, to detect thrash
    this.everRequested = new Set(); // distinct keys ever acquired, for the upload-once proof
  }

  get residentTiles() { return this.map.size; }
  get residentBytes() { return this.map.size * TILE_BYTES; }

  /**
   * Look a tile up for drawing. Never blocks: returns the texture if resident (and marks
   * it used), otherwise schedules a fetch and returns null so the pane draws GREY.
   * Grey-for-missing is not a fallback here, it is the honest answer.
   */
  acquire(tile) {
    const key = tileKey(tile);
    this.stats.requests++;
    this.everRequested.add(key);
    const e = this.map.get(key);
    if (e) { e.lastUsed = ++this.clock; this.stats.hits++; return e; }
    this.stats.misses++;
    this.schedule(tile);
    return null;
  }

  schedule(tile) {
    const key = tileKey(tile);
    if (this.map.has(key) || this.inflight.has(key)) return;
    if (this.queue.some((q) => tileKey(q) === key)) return;
    this.queue.push(tile);
    this.pump();
  }

  pump() {
    while (this.inflight.size < this.inflightCap && this.queue.length) {
      const tile = this.queue.shift();
      const key = tileKey(tile);
      if (this.map.has(key) || this.inflight.has(key)) continue;
      this.stats.fetches++;
      if (this.evicted.has(key)) this.stats.refetchAfterEvict++;
      const p = Promise.resolve(this.source(tile))
        .then((data) => { if (data) this.upload(tile, data); })
        .catch(() => {})
        .finally(() => { this.inflight.delete(key); this.pump(); });
      this.inflight.set(key, p);
    }
  }

  upload(tile, data) {
    const gl = this.gl, key = tileKey(tile);
    if (this.map.has(key)) return; // already there: never upload twice
    this.stats.fetchBytes += data.rgb.byteLength;
    const t0 = performance.now();
    const tex = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, tex);
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    gl.texStorage2D(gl.TEXTURE_2D, 1, gl.RGB8, 256, 256);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, 256, 256, gl.RGB, gl.UNSIGNED_BYTE, data.rgb);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    this.stats.uploadMs += performance.now() - t0;
    this.stats.uploads++;
    this.map.set(key, { key, tex, lastUsed: ++this.clock, pinned: !!tile.pinned, tile });
    this.evict();
  }

  /** Invalidate one tile so the live edge can rewrite it (§5.2: computed on request at the edge). */
  invalidate(tile) {
    const key = typeof tile === "string" ? tile : tileKey(tile);
    const e = this.map.get(key);
    if (!e) return false;
    this.gl.deleteTexture(e.tex);
    this.map.delete(key);
    return true;
  }

  evict() {
    while (this.map.size > this.budgetTiles) {
      let victim = null;
      for (const e of this.map.values()) {
        if (e.pinned) continue;
        if (!victim || e.lastUsed < victim.lastUsed) victim = e;
      }
      if (!victim) break;
      this.gl.deleteTexture(victim.tex);
      this.map.delete(victim.key);
      this.evicted.add(victim.key);
      this.stats.evictions++;
    }
  }

  dispose() {
    for (const e of this.map.values()) this.gl.deleteTexture(e.tex);
    this.map.clear();
  }
}
