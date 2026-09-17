// T-437 spike — the view scheme, and the de-welded per-axis addressing.
//
// docs/16 §6.2: 256x256 cells per tile at every level, x2 on BOTH axes, 8 levels.
// docs/16 §8.2: level_f and level_t are INDEPENDENT coordinates. Everything in this file
// takes them as two numbers and never derives one from the other. That is the whole point:
// 6 GHz of frequency and a retention window are different quantities and no single
// pixels-per-unit serves both.

export const CELLS = 256;          // cells per tile edge, both axes (§6.2)
export const MAX_LEVEL_F = 7;
export let MAX_LEVEL_T = 7;
export function setMaxLevelT(n) { MAX_LEVEL_T = n; }

// Level 0 cell sizes, from §6.2's table row V0.
// MUTABLE, because the spike's most important finding is that §6.2's V0 time cell (128 s)
// is far too coarse for "live is a viewport onto the finest level" to be true: every
// realistic pane resolves to level_t = 0 and the time axis stops being a ladder at all.
// `setLadder` lets the harness run the same renderer on a ladder that reaches down to the
// live edge, which is both the fix and the stress case.
// DEFAULT IS THE STORE'S REAL FLOOR (scheme 1 level 0: 6.25 kHz x 1 s), NOT docs/16
// §6.2's V0 (100 kHz x 128 s). Measured finding: at V0 the ENTIRE 120 s IQ retention
// window fits inside ONE 128 s time cell, so "live is a viewport onto the finest level"
// is unrepresentable on that ladder. See REPORT.md finding F1.
export const LADDER = { f0: 6.25e3, t0: 1e9 };
export function setLadder(fHz, tNs) { LADDER.f0 = fHz; LADDER.t0 = tNs; }

export const fCellHz = (lf) => LADDER.f0 * Math.pow(2, lf);
export const tCellNs = (lt) => LADDER.t0 * Math.pow(2, lt);
export const fBlockHz = (lf) => fCellHz(lf) * CELLS;
export const tBlockNs = (lt) => tCellNs(lt) * CELLS;

// Device extent (HackRF One). The canvas is virtual and spans exactly this in X.
export const F_LO_HZ = 1e6;
export const F_HI_HZ = 6e9;

/** Smallest level whose cell is still finer than the requested Hz-per-pixel. */
export function levelForHzPerPx(hzPerPx) {
  for (let l = 0; l <= MAX_LEVEL_F; l++) if (fCellHz(l) >= hzPerPx) return l;
  return MAX_LEVEL_F;
}
/** Same, on the time axis, with its own ladder. Deliberately a separate function. */
export function levelForNsPerPx(nsPerPx) {
  for (let l = 0; l <= MAX_LEVEL_T; l++) if (tCellNs(l) >= nsPerPx) return l;
  return MAX_LEVEL_T;
}

/**
 * Tiles covering a viewport box. `box` is absolute: { f0, f1 } Hz, { t0, t1 } ns.
 * Returns [{ lf, lt, fb, tb, f0, f1, t0, t1 }] with each tile's own absolute extent,
 * so the renderer never needs to know the ladder.
 */
export function tilesFor(box, lf, lt) {
  const fw = fBlockHz(lf), tw = tBlockNs(lt);
  const fb0 = Math.floor(box.f0 / fw), fb1 = Math.floor((box.f1 - 1e-9) / fw);
  const tb0 = Math.floor(box.t0 / tw), tb1 = Math.floor((box.t1 - 1e-9) / tw);
  const out = [];
  for (let tb = tb0; tb <= tb1; tb++)
    for (let fb = fb0; fb <= fb1; fb++)
      out.push({ lf, lt, fb, tb, f0: fb * fw, f1: (fb + 1) * fw, t0: tb * tw, t1: (tb + 1) * tw });
  return out;
}

export const tileKey = (t) => `${t.lf}/${t.lt}/${t.fb}/${t.tb}`;

/**
 * A pane is a viewport onto the one surface. Live is NOT a mode: `follow` pins the pane's
 * time window to the growing edge, which is the finest level where hardware is tuned.
 */
export function makePane(id, { centerF, spanF, centerT, spanT, follow = false, rect }) {
  return { id, centerF, spanF, centerT, spanT, follow, rect };
}

export function paneBox(p) {
  return {
    f0: p.centerF - p.spanF / 2, f1: p.centerF + p.spanF / 2,
    t0: p.centerT - p.spanT / 2, t1: p.centerT + p.spanT / 2,
  };
}

/** Per-axis level resolution for a pane, from its own pixel rectangle. Independent axes. */
export function paneLevels(p) {
  return {
    lf: levelForHzPerPx(p.spanF / Math.max(1, p.rect.w)),
    lt: levelForNsPerPx(p.spanT / Math.max(1, p.rect.h)),
  };
}

/** Clamp a pane to the device extent; time is clamped by the caller against the ring. */
export function clampPane(p) {
  const maxSpanF = F_HI_HZ - F_LO_HZ;
  p.spanF = Math.min(p.spanF, maxSpanF);
  p.centerF = Math.max(F_LO_HZ + p.spanF / 2, Math.min(F_HI_HZ - p.spanF / 2, p.centerF));
  return p;
}
