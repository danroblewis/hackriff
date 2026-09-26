// T-1006 (MMAP): **a pane's `device`, made visible and choosable.**
//
// docs/16 §8 gave every pane a `device` selector — *"a pane's `device` only chooses whose coverage
// decides its grey"* — and `surface/panes.ts` has carried it since the cutover (`PaneState.device`,
// `PaneModel.setDevice`, `coveringWindow`'s filter). Nothing on screen ever said what it was, and no
// control ever set it: with one radio the default `"any"` is that radio and the field is invisible;
// with two (MSDR: T-510 N capture sets, T-511 the `device_id` selector on every device route, T-512
// repeatable `--device`, T-514 the RTL-SDR) the user could neither see which front end a pane's grey
// came from nor tell a retune which radio to move.
//
// This module is the **pure arithmetic** behind that: what the pill says, what the picker offers,
// whether "one pane per device" is on the table, and — the load-bearing one — **which `device_id` a
// pane's retune must name**. It reaches no route, holds no DOM and knows nothing about tuning
// beyond the numbers `/api/control/state` already reported; `ui/test/surface-pane-device.test.ts`
// drives all of it.
//
// ## Why the retune's device is decided here and not at the request
//
// The backend's rule (docs/api.md, "Which radio: the device selector"): a device route with **no**
// `device_id` is *that* front end on a one-radio run and **`400 device_required`** on a multi-radio
// one — *"where nothing was said, nothing is invented"*. A client that just posts and hopes turns
// that honest refusal into a failed user action. So [[retuneDevice]] answers the question **before**
// the offer is painted, and a pane whose device cannot be resolved says so on its own row
// (`retune.ts`'s `device_required` / `device_gone` blocks) instead of offering a press that cannot
// land. Same discipline as `RetunePlan`'s refusals: stated and disabled, never clamped into
// something takeable.

/** One front end this run holds, as `/api/control/state`'s `devices[]` reduced for display. */
export interface AttachedDevice {
  /** The provenance `device_id` — the selector every device route takes, and the only field
   * guaranteed unique (the server refuses two front ends reporting the same one). */
  readonly id: string;
  /** The source driver's own name (`hackrf-one`, `rtl-sdr`, `mock-sdr`, …), never prettified here. */
  readonly driver: string;
  /** This front end's own tuned centre / span, or null when it reported none. */
  readonly centerHz: number | null;
  readonly sampleRateHz: number | null;
}

/** The pane-device value meaning "the union of every front end" — `PaneState.device`'s default. */
export const ANY_DEVICE = "any";

/**
 * A driver name as a user reads it. Unknown drivers come back **unchanged**: a front end this UI
 * has no word for is named by the word the backend used, never by a guess or a placeholder.
 */
export function driverLabel(driver: string): string {
  switch (driver) {
    case "hackrf-one": return "HackRF";
    case "rtl-sdr": return "RTL-SDR";
    case "mock-sdr": return "Mock SDR";
    case "sigmf-replay": return "Replay";
    default: return driver;
  }
}

/** `2400000` → `"2.4 Msps"`; null → `null` (nothing said, so nothing is said). */
export function rateLabel(sampleRateHz: number | null): string | null {
  if (sampleRateHz === null || !Number.isFinite(sampleRateHz) || sampleRateHz <= 0) return null;
  const msps = sampleRateHz / 1e6;
  return `${msps >= 10 ? msps.toFixed(1) : msps.toFixed(3).replace(/0+$/, "").replace(/\.$/, "")} Msps`;
}

/**
 * How one front end is named on screen: `"HackRF · 2.4 Msps"`, or just `"RTL-SDR"` when it reports
 * no rate.
 *
 * `disambiguate` appends the tail of the `device_id` when two attached front ends would otherwise
 * read identically (two mock SDRs, two HackRFs at the same rate). The id is the only unique fact,
 * so it is what breaks the tie — a numbered label ("HackRF 2") would name a thing the backend never
 * said and would renumber itself when a radio was unplugged.
 */
export function deviceLabel(d: AttachedDevice, disambiguate = false): string {
  const rate = rateLabel(d.sampleRateHz);
  const base = rate ? `${driverLabel(d.driver)} · ${rate}` : driverLabel(d.driver);
  return disambiguate ? `${base} (${shortDeviceId(d.id)})` : base;
}

/** The tail of a `device_id`, for a label that has to break a tie: `mock:hackrf:0000…c293`. */
export function shortDeviceId(id: string): string {
  const head = id.includes(":") ? `${id.slice(0, id.indexOf(":"))}:` : "";
  const tail = id.slice(-4);
  return id.length <= head.length + 4 ? id : `${head}…${tail}`;
}

/** Every attached front end's display label, disambiguated only where two would collide. */
export function deviceLabels(attached: readonly AttachedDevice[]): Map<string, string> {
  const plain = attached.map((d) => deviceLabel(d, false));
  const out = new Map<string, string>();
  attached.forEach((d, i) => {
    const collides = plain.some((l, j) => j !== i && l === plain[i]);
    out.set(d.id, deviceLabel(d, collides));
  });
  return out;
}

/** The pill on a pane's status row: what it says, and the sentence behind it. */
export interface DevicePill {
  /** The selector as state, for a stylesheet and a test: a `device_id`, or `"any"`. */
  readonly device: string;
  readonly label: string;
  /** The whole sentence — whose coverage decides this pane's grey, and which radio it would move. */
  readonly why: string;
  /** True when the pane is pinned to a `device_id` this run does not hold. */
  readonly stale: boolean;
}

/**
 * The pill for one pane.
 *
 * With **one** attached front end and a pane on `"any"` the pill names **that radio**, because the
 * union of one front end *is* that front end — saying "any" there would hide the only fact the pill
 * exists to state. With several, `"any"` is named as the union, which is the honest answer and the
 * one the retune then refuses on.
 *
 * A pane pinned to a `device_id` that is no longer held is **said so**, never silently reset to
 * `"any"`: the pane's grey is that radio's grey, and a reset would turn "we have no coverage from
 * the radio you asked about" into "we looked with something".
 */
export function devicePill(attached: readonly AttachedDevice[], paneDevice: string): DevicePill {
  const labels = deviceLabels(attached);
  if (paneDevice !== ANY_DEVICE) {
    const d = attached.find((a) => a.id === paneDevice);
    if (!d) {
      return {
        device: paneDevice, label: `${shortDeviceId(paneDevice)} — not attached`, stale: true,
        why: `This viewport is pinned to ${paneDevice}, which this run does not hold: its grey is that front end's coverage, and nothing here can retune it. Pick a front end in the viewport menu.`,
      };
    }
    return {
      device: d.id, label: labels.get(d.id)!, stale: false,
      why: `This viewport's coverage — where it is grey — is ${d.id}'s alone, and a retune here moves that front end.`,
    };
  }
  if (attached.length === 1) {
    const d = attached[0];
    return {
      device: ANY_DEVICE, label: labels.get(d.id)!, stale: false,
      why: `One front end is attached (${d.id}), so this viewport's coverage is its coverage and a retune here moves it.`,
    };
  }
  if (attached.length === 0) {
    return {
      device: ANY_DEVICE, label: "No front end", stale: false,
      why: "No live front end is attached (a replay, or a run with none): this viewport's coverage is whatever was recorded, and nothing here can retune.",
    };
  }
  return {
    device: ANY_DEVICE, label: `Any of ${attached.length}`, stale: false,
    why: `This viewport's grey is the union of every front end (${attached.map((d) => d.id).join(", ")}). A retune must name one radio, so pick a front end in the viewport menu before retuning here.`,
  };
}

/** One row of the pane's device picker — the same strings-and-a-bit shape the layers menu uses. */
export interface DeviceRow {
  /** `"any"` or a `device_id`, handed straight back to the host's setter. */
  readonly id: string;
  readonly label: string;
  readonly hint: string;
  readonly on: boolean;
}

/** The picker's rows for one pane: the union first, then every attached front end in composition
 * order, plus the pane's own stale pin when it names a radio this run no longer holds. */
export function deviceRows(attached: readonly AttachedDevice[], paneDevice: string): DeviceRow[] {
  const labels = deviceLabels(attached);
  const rows: DeviceRow[] = [{
    id: ANY_DEVICE,
    label: attached.length === 1 ? `Any front end (${labels.get(attached[0].id)})` : "Any front end",
    hint: attached.length > 1
      ? "grey is the union of every radio; a retune must name one"
      : "the union of every radio's coverage",
    on: paneDevice === ANY_DEVICE,
  }];
  for (const d of attached) {
    rows.push({
      id: d.id, label: labels.get(d.id)!, hint: d.id,
      on: paneDevice === d.id,
    });
  }
  if (paneDevice !== ANY_DEVICE && !attached.some((d) => d.id === paneDevice)) {
    rows.push({ id: paneDevice, label: `${shortDeviceId(paneDevice)} — not attached`, hint: paneDevice, on: true });
  }
  return rows;
}

/** "One pane per device": offered only when there is more than one front end to spread. */
export interface SplitPerDeviceOffer {
  readonly enabled: boolean;
  readonly label: string;
  readonly why: string;
  /** The devices, in composition order, one pane each — `[]` when the offer is refused. */
  readonly devices: readonly string[];
}

/**
 * The split-view offer the ticket asks for: with two front ends attached, one pane per radio.
 *
 * Stated-and-disabled with one radio rather than hidden (the [[RowAction]] rule): a control that
 * vanishes teaches nothing, and "there is only one radio" is exactly what a user wondering where
 * their second SDR went needs to read.
 */
export function splitPerDeviceOffer(attached: readonly AttachedDevice[]): SplitPerDeviceOffer {
  const labels = deviceLabels(attached);
  if (attached.length < 2) {
    return {
      enabled: false, label: "One viewport per front end", devices: [],
      why: attached.length === 1
        ? `Only ${labels.get(attached[0].id)} is attached, so there is nothing to spread across viewports.`
        : "No live front end is attached, so there is nothing to spread across viewports.",
    };
  }
  return {
    enabled: true, label: "One viewport per front end", devices: attached.map((d) => d.id),
    why: `${attached.length} viewports, one pinned to each front end (${attached.map((d) => labels.get(d.id)!).join(", ")}): each then draws its own radio's coverage and retunes its own radio. A view change — no radio moves.`,
  };
}

/**
 * **Which front end a retune from this pane must name.**
 *
 * `"named"` carries the `device_id` to put in the request body. `"default"` means send no selector:
 * a run with no live front end at all, where the route's own `not_live` is the right answer and a
 * made-up id would only turn it into `unknown_device`. The two refusals mirror the backend's:
 *
 *  - `"ambiguous"` — the pane is on `"any"` and this run holds several radios, so the request would
 *    be `400 device_required`. The user picks one; the client does not pick for them (docs/api.md:
 *    *"a default costs the user a band they were listening to"*).
 *  - `"gone"` — the pane names a `device_id` this run does not hold (`404 unknown_device`).
 */
export type RetuneDevice =
  | { readonly kind: "named"; readonly deviceId: string }
  | { readonly kind: "default" }
  | { readonly kind: "ambiguous"; readonly ids: readonly string[] }
  | { readonly kind: "gone"; readonly requested: string; readonly ids: readonly string[] };

export function retuneDevice(attached: readonly AttachedDevice[], paneDevice: string): RetuneDevice {
  const ids = attached.map((d) => d.id);
  if (paneDevice !== ANY_DEVICE) {
    return ids.includes(paneDevice)
      ? { kind: "named", deviceId: paneDevice }
      : { kind: "gone", requested: paneDevice, ids };
  }
  if (attached.length === 1) return { kind: "named", deviceId: ids[0] };
  if (attached.length === 0) return { kind: "default" };
  return { kind: "ambiguous", ids };
}
