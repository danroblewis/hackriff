// T-997 (MMAP): where the two inventory pills live — a one-element registry, so the pills
// (`inv-pills.ts`, mounted with Explore's areas) and their home in the map's floating chrome
// (`map-controls.ts`, built by the surface when it mounts) can arrive in either order, and the
// pills re-home themselves if the surface remounts its cluster.
//
// Presentation only: it holds an element and a callback, nothing else. No store, no client.

let home: HTMLElement | null = null;
let fill: ((el: HTMLElement) => void) | null = null;

/** The map's floating chrome has built (or rebuilt) the pills' row. */
export function registerMapInvHome(el: HTMLElement): void {
  home = el;
  if (fill) fill(el);
}

/** The pills' mount: fill the row now if it exists, and again whenever it is rebuilt. */
export function fillMapInvHome(fn: (el: HTMLElement) => void): void {
  fill = fn;
  if (home) fn(home);
}

/** Test seam: forget both sides (each test mounts its own). */
export function resetMapInvHome(): void {
  home = null;
  fill = null;
}
