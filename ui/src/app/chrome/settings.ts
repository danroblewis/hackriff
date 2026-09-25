// T-1007 (MMAP): the ⋯ settings menu — what the user said was in Review but is really settings.
//
// User, 2026-09-25: "instead of the Review section a lot of those things could be considered
// settings." So the small ⋯ menu T-993 opened for Theme is now the one home for every *preference*
// on the map, and Review keeps only what is review: anomalies/alarms and its badge (`review/slice.ts`
// splits the drawer's tabs into the two groups).
//
// What lives here, and why each one is a setting rather than a panel:
//
//   Theme          — the moved `#theme-btn` node itself (T-993 put it here; unchanged).
//   Colour scale   — one radio group over `surface/contrast.ts`'s three range modes, rehomed from
//                    the layers menu. It is view-WIDE and it is the contrast control: "auto-contrast"
//                    and "viewport scale" are two of its three modes, so offering them separately
//                    would be two controls over one piece of state.
//   Time ruler     — whether the HUD's time ruler reads seconds-ago or the local clock time
//                    (T-998's `surface/hud.ts` `TimeLabelMode`, one radio group over it). Labelling
//                    only: the marks are at the same capture instants either way.
//   Front ends     — every live device from `GET /api/control/state`'s `devices` (T-511), and which
//                    one's coverage each pane's grey is decided by (`PaneModel.setDevice` — panes are
//                    where you look *from*, so a device choice here never adds a view window).
//   Capture window — the backend's CONFIGURED retention and what the IQ ring currently holds, read
//                    from `GET /api/timeline`'s `window` (the capture clock). Stated, not edited:
//                    resizing the ring is a server-side setting (`--iq-retention`), and there is no
//                    route for it, so the menu says where it is set instead of implying a control.
//
// Thin client (CLAUDE.md): every press here is presentation state — a display range, a label form, a
// pane's coverage source, a stored preference. Nothing on this path reaches a device route, and the
// device list and the capture window are the backend's own words, re-read, never computed here.
import type { TimeLabelMode } from "../../surface/hud";
import { h } from "../dom";

export type { TimeLabelMode };

/** The two ruler rows, derived from the mode in force — never tracked beside it. The mode itself is
 * T-998's (`surface/hud.ts`'s `getTimeLabelMode`/`setTimeLabelMode`, stored as `hk-hud-time-labels`):
 * this menu is where it is chosen, not a second copy of it. */
export function rulerRows(mode: TimeLabelMode): { id: TimeLabelMode; label: string; hint: string; on: boolean }[] {
  return [
    { id: "relative", label: "Seconds ago", hint: "−1m20s behind the live edge", on: mode === "relative" },
    { id: "absolute", label: "Clock time", hint: "the capture instant, local time", on: mode === "absolute" },
  ];
}

/** One radio/checkbox row of a settings group. The same shape the layers menu's rows use. */
export interface SettingsRow { id: string; label: string; hint: string; on: boolean }

/** One live front end, as the menu names it. Every field is the backend's, or null when it said nothing. */
export interface SettingsDevice {
  /** The provenance `device_id` — the key a pane's coverage is read by. Null = the source named none. */
  id: string | null;
  driver: string;
  kind: "hardware" | "replay";
  /** What it is tuned to now, in words, or null when the state has not said. */
  tuned: string | null;
}

/** One pane, and which device's coverage decides its grey. */
export interface SettingsPane { id: string; label: string; device: string }

/** Everything the menu shows, re-derived each time it opens (and while it is open). */
export interface SettingsModel {
  /** The colour scale (= contrast) modes, view-wide, and the sentence stating the range in force. */
  scale: { rows: SettingsRow[]; note: string };
  ruler: { rows: SettingsRow[] };
  devices: {
    list: SettingsDevice[];
    /** Why the list is empty, when it is (a replay has no live front end). Null when it is not. */
    empty: string | null;
    panes: SettingsPane[];
    /** The choices a pane's coverage source offers: `any` plus one per device. */
    choices: { id: string; label: string }[];
  };
  /** The bigger settings panels, which open in the shared drawer: the device/display/sweep controls,
   * the scheduler, the saved bookmarks. They used to be Review tabs; Review keeps only what is review
   * (T-1007). Each id is opaque here — the host knows which panel it names. */
  panels: { id: string; label: string; hint: string }[];
  capture: {
    /** The configured retention in words, or null when the backend has not stated one. */
    retention: string | null;
    /** What the ring holds inside it, in words; null when it holds nothing or said nothing. */
    buffered: string | null;
    /** Where the retention is set — it is a server setting, not a control on this menu. */
    note: string;
  };
}

/** The menu's writes. Presentation state only; none of them can reach a route. */
export interface SettingsHost {
  settings(): SettingsModel;
  /** Choose the colour-scale/contrast mode (a display range, never a gain). */
  setScale(id: string): void;
  setRulerMode(mode: TimeLabelMode): void;
  /** Which device's coverage decides this pane's grey (`any` = the union). */
  setPaneDevice(paneId: string, device: string): void;
  /** Open one of the drawer's settings panels. Opening a panel is view state; what the panel itself
   * then does (a gain, a sweep, a schedule) is the panel's own, gated, business. */
  openPanel(id: string): void;
}

/**
 * Render the settings into `list`, replacing what was there.
 *
 * Re-rendered after every press (the rows are derived from the state, so a press is visible only by
 * re-deriving), and keyboard focus is kept on the control that was pressed — the layers menu's rule.
 */
export function renderSettings(list: HTMLElement, host: SettingsHost, close: () => void = () => {}): void {
  const m = host.settings();
  const act = document.activeElement as HTMLElement | null;
  const refocus = act && list.contains(act)
    ? (act.dataset.scale ? `[data-scale="${act.dataset.scale}"]`
      : act.dataset.ruler ? `[data-ruler="${act.dataset.ruler}"]`
        : act.dataset.paneDevice ? `[data-pane-device="${act.dataset.paneDevice}"]` : null)
    : null;
  const again = () => renderSettings(list, host, close);
  const radio = (group: string, attr: string, r: SettingsRow, press: () => void) => {
    const input = h("input", { type: "radio", name: group, value: r.id, [attr]: r.id }) as HTMLInputElement;
    input.checked = r.on;
    input.addEventListener("change", () => { press(); again(); });
    return h("label", { class: "map-row" }, input, r.label, h("small", {}, r.hint));
  };
  const group = (axis: string, title: string, label: string, role: string, ...body: (HTMLElement | string)[]) =>
    h("div", { class: "map-layers-axis", "data-axis": axis, role, "aria-label": label }, h("h4", {}, title), ...body);

  const dev = m.devices;
  list.replaceChildren(
    group("scale", "Colour scale · every pane · one at a time", "Colour scale, every pane", "radiogroup",
      ...m.scale.rows.map((r) => radio("map-scale", "data-scale", r, () => host.setScale(r.id))),
      h("div", { class: "map-layers-note sf-range-note" }, m.scale.note)),
    group("ruler", "Time ruler · every pane", "Time ruler labels", "radiogroup",
      ...m.ruler.rows.map((r) => radio("map-ruler", "data-ruler", r, () => host.setRulerMode(r.id as TimeLabelMode)))),
    group("devices", "Front ends", "Front ends and which viewport reads which", "group",
      ...(dev.empty !== null ? [h("div", { class: "map-layers-note" }, dev.empty)] : []),
      ...dev.list.map((d) => h("div", { class: "map-device", "data-device": d.id ?? "" },
        h("span", { class: "map-device-name" }, d.driver),
        h("small", {}, d.tuned ?? "not tuned"),
        h("div", { class: "map-layers-note mono" }, d.id ?? "this source reports no device id"))),
      ...dev.panes.map((p) => {
        const sel = h("select", { "data-pane-device": p.id, "aria-label": `Coverage shown on ${p.label}` },
          ...dev.choices.map((c) => {
            const o = h("option", { value: c.id }, c.label) as HTMLOptionElement;
            if (c.id === p.device) o.selected = true;
            return o;
          })) as HTMLSelectElement;
        sel.value = p.device;
        sel.addEventListener("change", () => { host.setPaneDevice(p.id, sel.value); again(); });
        return h("label", { class: "map-row map-row-sel" }, h("span", {}, `${p.label} grey`), sel);
      }),
      h("div", { class: "map-layers-note" },
        "A front end chooses whose coverage decides a viewport's grey — never a second view window. "
        + "Extra radios widen the coverage available to show; they never add a place to look.")),
    group("panels", "More settings", "More settings", "group",
      ...m.panels.map((p) => {
        const b = h("button", { type: "button", class: "map-pane-item", "data-panel": p.id, title: p.hint },
          `${p.label}…`) as HTMLButtonElement;
        // A panel opens in the drawer beside the map, so the little menu gets out of the way — the
        // same manners as the viewport menu's items.
        b.addEventListener("click", () => { host.openPanel(p.id); close(); });
        return b;
      })),
    group("capture", "Capture window", "Capture window", "group",
      h("div", { class: "map-row map-row-read" }, h("span", {}, "Retention"),
        h("strong", { class: "map-capture-retention" }, m.capture.retention ?? "not stated")),
      ...(m.capture.buffered !== null
        ? [h("div", { class: "map-row map-row-read" }, h("span", {}, "IQ held"),
          h("strong", { class: "map-capture-buffered" }, m.capture.buffered))]
        : []),
      h("div", { class: "map-layers-note" }, m.capture.note)),
    h("div", { class: "map-layers-note" },
      "Settings change what is shown and how it is labelled — never what is captured, measured or detected."),
  );
  if (refocus) (list.querySelector(refocus) as HTMLElement | null)?.focus();
}
