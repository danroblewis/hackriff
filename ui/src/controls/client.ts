// Control API client (T-051 over T-050, crates/hk-api/src/control.rs). Pure request building and
// error policy, unit-tested in ui/test/controls.test.ts.
//
// Token rules (T-050):
// - Every fetch carries `Authorization: Bearer <token>`; mutating calls (POST, PUT, DELETE) must,
//   and the server refuses `?token=` for them, so a mutating URL never contains the token.
// - URLs are origin-relative (`/api/...`): through the cloudflared tunnel the page is https and
//   every call stays on that origin (the server refuses cross-origin control calls).

export type Method = "GET" | "POST" | "PUT" | "DELETE";

export const isMutating = (m: Method) => m !== "GET";

/** A request ready for `fetch`. */
export interface BuiltRequest {
  url: string;
  init: RequestInit & { method: Method; headers: Record<string, string> };
}

/** Builds a request. Throws on a non-origin-relative path or a token in the URL. */
export function buildRequest(method: Method, path: string, token: string, body?: unknown): BuiltRequest {
  if (!path.startsWith("/") || path.startsWith("//")) throw new Error(`control path must be origin-relative: ${path}`);
  if (/[?&]token=/.test(path)) throw new Error("the token never goes in a fetch URL; it is sent as a header");
  const headers: Record<string, string> = { Authorization: `Bearer ${token}` };
  const init: BuiltRequest["init"] = { method, headers, cache: "no-store", credentials: "same-origin" };
  if (body !== undefined) {
    if (!isMutating(method)) throw new Error("GET requests carry no body");
    headers["Content-Type"] = "application/json";
    init.body = JSON.stringify(body);
  }
  return { url: path, init };
}

/** An API error: HTTP status, the server's stable `code`, and its message. */
export class ControlError extends Error {
  constructor(readonly status: number, readonly code: string, message: string) {
    super(message);
    this.name = "ControlError";
  }
}

/** The error for a non-2xx response body (`{"error", "code"}`; 401 bodies carry no code). */
export function errorFrom(status: number, body: unknown, statusText = ""): ControlError {
  const b = (body ?? {}) as { error?: unknown; code?: unknown };
  const code = typeof b.code === "string" ? b.code : status === 401 ? "unauthorized" : status === 403 ? "forbidden" : `http_${status}`;
  const msg = typeof b.error === "string" ? b.error : statusText || `HTTP ${status}`;
  return new ControlError(status, code, msg);
}

export type FetchFn = (url: string, init: RequestInit) => Promise<{ ok: boolean; status: number; statusText: string; json(): Promise<unknown> }>;

/** Calls the API with the tab's token. */
export class ControlClient {
  constructor(private token: string, private fetchFn: FetchFn = (u, i) => fetch(u, i)) {}

  async call<T = unknown>(method: Method, path: string, body?: unknown): Promise<T> {
    // Mutating calls always send a JSON object (`{}` when empty): the server accepts it and the
    // Content-Type is then unambiguous.
    const req = buildRequest(method, path, this.token, isMutating(method) && method !== "DELETE" ? body ?? {} : body);
    const r = await this.fetchFn(req.url, req.init);
    const data = await r.json().catch(() => ({}));
    if (!r.ok) throw errorFrom(r.status, data, r.statusText);
    return data as T;
  }

  get<T = unknown>(path: string) { return this.call<T>("GET", path); }
  post<T = unknown>(path: string, body?: unknown) { return this.call<T>("POST", path, body); }
  put<T = unknown>(path: string, body: unknown) { return this.call<T>("PUT", path, body); }
  del<T = unknown>(path: string) { return this.call<T>("DELETE", path); }
}

/** What the UI does about a failed call. */
export type Reaction =
  | "reauth"      // 401: ask for the token again
  | "not-live"    // 409 not_live: disable device controls (replay)
  | "busy"        // 409 conflict / device_busy: a re-plumb, a recording, or another holder of the radio
  | "refused"     // 409 refused: legal/class gating said no (show the reason)
  | "pending"     // 504 timeout: the re-plumb continues; state polling catches up
  | "finished"    // 409 finished: the run ended
  | "field"       // 400 invalid / out_of_range: the value was wrong
  | "unsupported" // 501: the device lacks the capability
  | "offline"     // network failure
  | "error";      // anything else

/** Maps an error to a reaction and a message for the status line. */
export function reactionTo(e: unknown): { reaction: Reaction; message: string } {
  if (!(e instanceof ControlError)) {
    return { reaction: "offline", message: `server unreachable: ${e instanceof Error ? e.message : String(e)}` };
  }
  const m = e.message;
  switch (e.code) {
    case "unauthorized": return { reaction: "reauth", message: "token rejected: paste the token again" };
    case "not_live": return { reaction: "not-live", message: "replay: device settings need a live source (display, pause, record and bookmarks still work)" };
    case "conflict": return { reaction: "busy", message: `busy: ${m}` };
    // T-343: only one process can hold the radio, so a device action can lose it. Say who has it
    // and stop; never retry a device command into a race.
    case "device_busy": return { reaction: "busy", message: `radio busy: ${m}` };
    case "refused": return { reaction: "refused", message: `refused: ${m}` };
    case "timeout": return { reaction: "pending", message: `still re-plumbing: ${m}` };
    case "finished": return { reaction: "finished", message: `run finished: ${m}` };
    case "invalid": case "out_of_range": return { reaction: "field", message: m };
    case "unsupported": return { reaction: "unsupported", message: m };
    default: return { reaction: "error", message: `${e.status} ${e.code}: ${m}` };
  }
}
