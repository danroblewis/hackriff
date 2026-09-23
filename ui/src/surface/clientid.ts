// **Who this page is, as far as `GET /api/tiles` is concerned** (T-630).
//
// The route's in-flight cap is ingest backpressure and is server-wide. Before T-630 it was also
// first-come-first-served, so one tab enumerating a wide viewport held all four slots continuously
// — it re-asks the instant one frees — and a second tab's *first* request, the one it cannot start
// without, competed on equal terms with the thousandth request of a tab that is already drawn.
// Measured: the second tab booted in 8.2 s and 11.7 s after 7 refusals, and twice did not boot at
// all.
//
// The fix is a per-client share of the cap, which needs a client identity on the route. It is a
// **declared** id rather than a connection or a session, for two reasons the other two cannot
// meet: a tab's tile reads go over a pool of connections (so a connection is not a client), and
// every tab of one browser carries the same API token (so a session is not a client either).
//
// **Fresh per page load, not remembered.** `sessionStorage` would survive a reload and hand the
// new page the old one's identity — including the reads the old page abandoned, which the server
// still counts until they finish. A reloaded page is exactly the client this policy calls a
// newcomer, so it gets a new name and the bootstrap reserve with it.
//
// Nothing here is signal logic: it is a name this client gives itself so the backend's policy can
// be expressed. The policy is entirely the backend's (`crates/hk-api/src/tiles.rs`).

let id = "";

/** An opaque, per-page id. Uses `crypto.randomUUID` where it exists and a random string elsewhere. */
export function newClientId(): string {
  const c = (globalThis as { crypto?: Crypto }).crypto;
  if (c && typeof c.randomUUID === "function") return c.randomUUID();
  return `c-${Math.random().toString(36).slice(2)}${Date.now().toString(36)}`;
}

/** The id every tile request carries, or `""` before a host has named this page. */
export function tileClientId(): string {
  return id;
}

/**
 * Name this page, once, before it asks for its first tile.
 *
 * Unset is a legitimate state and is what every unit test runs in: an undeclared client shares the
 * route's anonymous bucket, which behaves exactly as the route did before T-630.
 */
export function setTileClientId(next: string): void {
  id = next;
}
