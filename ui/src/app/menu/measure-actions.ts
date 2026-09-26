// **What a measurement box can DO** (T-1009). The Measure tool (T-822) lets a user draw an
// arbitrary box on the canvas; until now that box was a saved *reading* and nothing else. The user:
// *"We can draw arbitrary boxes with the Measure tool; it would be great if those could choose one
// of the SDRs to start a scan."* So a measurement box gets the same right-click / long-press menu a
// detection box got in T-994, carrying the three acts a drawn region affords:
//
//  - **Scan this region with &lt;device&gt;** — open the scan plan (T-1008) bounded by the box's
//    FREQUENCY extent, on the radio the user picked. The box's time extent is not a scan parameter:
//    a live sweep walks frequency, and the region's time extent is when it was *measured*, not a
//    duration to sweep for. The plan is still the backend's (`GET /api/control/scan`) and Start is
//    still the one commissioning press — this menu chooses the region and the radio, nothing else.
//  - **Record IQ of this region with &lt;device&gt;** — `POST /api/iqbuffer/clip` over the box's
//    time window, band-filtered to its frequency extent, from that radio's own ring (T-1009 added
//    the `device_id` selector; every front end keeps its own ring, so this is a real choice).
//  - **Save as marker** — the durable time–frequency place (`POST /api/collections/{id}/markers`,
//    T-817), so a region worth coming back to survives the session and the Research panel lists it.
//
// **Thin client** (CLAUDE.md): every item is an existing route with the box's own coordinates in the
// body. Nothing here computes a step, a plan or a value; the scan item reaches no device route at
// all (it opens a plan the user must Start), and the marker/clip items are the server's own writes.
import type { AppContext } from "../context";
import type { AttachedDevice } from "../../surface/panedevice";
import type { MarkMeasurement } from "../../surface/marks";
import type { MeasureView } from "../explore/measure";
import { apiErrorText } from "../explore/format";
import { toast } from "../state";
import type { MenuItem } from "./model";

/** A device as this menu names it: the id the routes take, and the words a user reads. */
export interface MenuDevice {
  readonly id: string;
  readonly label: string;
}

/** The reserved `Bookmarks` collection (docs/api.md): frequency-only markers, so a measurement —
 * which has a time extent — is never filed there. */
export const BOOKMARKS_COLLECTION = "00000000-0000-7000-8000-000000000b00";
/** The collection a saved measurement-box marker goes to when the run has no other one yet. */
export const MEASURE_COLLECTION_NAME = "Marked regions";

const mhz = (hz: number) => (hz / 1e6).toFixed(3);

/** The client surface these actions need (a test double implements these three). */
export interface MeasureMenuClient {
  get<T = unknown>(path: string): Promise<T>;
  post<T = unknown>(path: string, body?: unknown): Promise<T>;
}

/** What the mount lends this menu: the radios to offer, the pane's view stamp for a write's
 * provenance, and the one act that is not an API call — opening the scan plan over the box. */
export interface MeasureMenuHost {
  /** Every live front end, as `/api/control/state`'s `devices` enumerates them. Empty on a replay. */
  devices: readonly MenuDevice[];
  /** The pane's own view stamp (`center_hz`/`span_hz`/`t_capture`/`tier`), or null. */
  view: MeasureView | null;
  /** Open the scan plan bounded by `[loHz, hiHz]` on `deviceId` (null = the run's default radio).
   * View + panel state; the plan is priced by a `GET` and nothing moves until Start. */
  openScan(region: { loHz: number; hiHz: number }, deviceId: string | null): void;
  /** Prompt for a marker's name (the mount passes `window.prompt`, a test passes a stub). */
  prompt(message: string, initial: string): string | null;
}

/** The devices a menu offers, from the shell's `devices` list and the same label map the pane's
 * device menu uses (`surface/panedevice.ts`), so one radio reads the same everywhere. A front end
 * the map has no word for falls back to its own id — never a placeholder. */
export function menuDevices(devices: readonly AttachedDevice[], labels: ReadonlyMap<string, string>): MenuDevice[] {
  return devices.map((d) => ({ id: d.id, label: labels.get(d.id) ?? d.id }));
}

/** The clip body for a measurement box: its time window, band-filtered to its frequency extent, off
 * the named radio's ring. Exactly the box's own coordinates — no widening, no rounding. */
export function clipRequest(m: MarkMeasurement, deviceId: string | null) {
  return {
    t0: m.t0_s, t1: m.t1_s,
    band: { f_lo: m.f_lo_hz, f_hi: m.f_hi_hz },
    label: `measured ${mhz(m.f_lo_hz)}–${mhz(m.f_hi_hz)} MHz`,
    ...(deviceId ? { device_id: deviceId } : {}),
  };
}

/** The marker body for a measurement box (docs/api.md "Marker collections"): a time–frequency BOX —
 * centre and width in frequency, centre and duration in time — so the mark keeps the extent the user
 * drew rather than collapsing to a pin. The server computes `f_lo_hz`/`f_hi_hz`/`t_start_s`/`t_end_s`
 * and stamps provenance from `view`. */
export function markerRequest(m: MarkMeasurement, name: string, view: MeasureView) {
  return {
    name,
    f_center_hz: (m.f_lo_hz + m.f_hi_hz) / 2,
    bandwidth_hz: m.f_hi_hz - m.f_lo_hz,
    t_center_s: (m.t0_s + m.t1_s) / 2,
    duration_s: m.t1_s - m.t0_s,
    view,
  };
}

/** The default name offered for a marker: the place, in the words the readout uses. */
export const markerName = (m: MarkMeasurement): string =>
  `${mhz(m.f_lo_hz)}–${mhz(m.f_hi_hz)} MHz · ${(m.t1_s - m.t0_s).toFixed(1)} s`;

/**
 * The collection a marker is filed in: the run's first non-reserved collection, or a new
 * **"Marked regions"** one when it has none. The reserved `Bookmarks` collection is never it —
 * a bookmark has no time, and a measurement box is a time–frequency place.
 */
export async function collectionForMarkers(client: MeasureMenuClient): Promise<string> {
  const list = await client.get<{ collections?: { id: string; name: string; reserved?: boolean }[] }>("/api/collections");
  const own = (list.collections ?? []).find((c) => !c.reserved && c.id !== BOOKMARKS_COLLECTION);
  if (own) return own.id;
  const made = await client.post<{ id: string }>("/api/collections", { name: MEASURE_COLLECTION_NAME });
  return made.id;
}

/** Saves `m` as a durable marker and reports what happened. Returns the marker's id, or null. */
export async function saveAsMarker(
  ctx: AppContext, client: MeasureMenuClient, m: MarkMeasurement, name: string, view: MeasureView,
): Promise<string | null> {
  try {
    const collection = await collectionForMarkers(client);
    const saved = await client.post<{ id: string }>(
      `/api/collections/${encodeURIComponent(collection)}/markers`, markerRequest(m, name, view));
    ctx.store.set(toast(`Saved marker: ${name}`));
    return saved.id ?? null;
  } catch (e) {
    ctx.store.set(toast(`Save as marker: ${apiErrorText(e)}`));
    return null;
  }
}

/** Records the region's IQ off `deviceId`'s ring and reports what happened. */
export async function recordRegionIq(
  ctx: AppContext, client: MeasureMenuClient, m: MarkMeasurement, deviceId: string | null, deviceLabel: string,
): Promise<void> {
  try {
    const r = await client.post<{ recording?: { id?: string; samples?: number } }>(
      "/api/iqbuffer/clip", clipRequest(m, deviceId));
    const n = r.recording?.samples;
    ctx.store.set(toast(`Recorded ${mhz(m.f_lo_hz)}–${mhz(m.f_hi_hz)} MHz from ${deviceLabel}${n ? ` (${n} samples)` : ""}`));
  } catch (e) {
    ctx.store.set(toast(`Record IQ: ${apiErrorText(e)}`));
  }
}

/**
 * The menu for a right-clicked / long-pressed **measurement box**.
 *
 * With one front end each act is one item naming that radio; with several, one item per radio, so
 * the choice the user makes is the `device_id` that reaches the engine — never a default guessed
 * for them. On a replay (no front end at all) both device items are present and **disabled with the
 * reason**, rather than absent: "there is no radio here" is an answer, and a menu that silently
 * drops its actions leaves the user hunting for a control that was never there.
 */
export function measurementMenuItems(ctx: AppContext, m: MarkMeasurement, host: MeasureMenuHost): MenuItem[] {
  const client = ctx.client as unknown as MeasureMenuClient;
  const region = { loHz: m.f_lo_hz, hiHz: m.f_hi_hz };
  const band = `${mhz(m.f_lo_hz)}–${mhz(m.f_hi_hz)} MHz`;
  const items: MenuItem[] = [];
  const devices = host.devices;
  const one = devices.length === 1 ? devices[0] : null;

  const scanItem = (id: string | null, label: string, suffix: string): MenuItem => ({
    id: `scan${id ? `:${id}` : ""}`,
    label: `Scan this region${suffix}`,
    hint: `${band} · opens the plan on ${label}; nothing moves until Start`,
    onSelect: () => host.openScan(region, id),
  });
  const recordItem = (id: string | null, label: string, suffix: string): MenuItem => ({
    id: `record-iq${id ? `:${id}` : ""}`,
    label: `Record IQ of this region${suffix}`,
    hint: `${band} · from ${label}'s IQ ring, over this box's time window`,
    onSelect: () => { void recordRegionIq(ctx, client, m, id, label); },
  });

  if (devices.length === 0) {
    const why = "this server replays a recording: there is no front end";
    items.push({ id: "scan", label: "Scan this region", hint: why, disabled: true, onSelect: () => {} });
    items.push({ id: "record-iq", label: "Record IQ of this region", hint: why, disabled: true, onSelect: () => {} });
  } else if (one) {
    // One radio: the selector may be omitted, and the hint still names which radio it is about.
    items.push(scanItem(null, one.label, ""));
    items.push(recordItem(null, one.label, ""));
  } else {
    for (const d of devices) items.push(scanItem(d.id, d.label, ` with ${d.label}`));
    for (const d of devices) items.push(recordItem(d.id, d.label, ` with ${d.label}`));
  }

  items.push({
    id: "save-marker", label: "Save as marker",
    hint: host.view ? "durable: survives a reload, and lists in Research" : "the pane has no view to stamp it with",
    disabled: !host.view,
    onSelect: () => {
      const view = host.view;
      if (!view) return;
      const name = host.prompt("Name for this marker:", markerName(m))?.trim();
      if (name) void saveAsMarker(ctx, client, m, name.slice(0, 120), view);
    },
  });
  return items;
}
