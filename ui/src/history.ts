// Pure time-input helpers for the region-over-time views (/api/history T-017, /api/floor T-021).
// Rendering lives in ui/src/app/review/history.ts (MUI Review drawer, T-155).
export const utcInput = (s: number) => new Date(s * 1000).toISOString().slice(0, 19);
export const fromUtcInput = (v: string) => Date.parse(v.length === 16 ? `${v}:00Z` : `${v}Z`) / 1000;
export const fmtT = (s: number) => new Date(s * 1000).toISOString().replace("T", " ").slice(0, 19);
