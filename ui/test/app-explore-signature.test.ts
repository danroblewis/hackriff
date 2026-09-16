// T-207 (ADR-0016 §5): signature-match and cluster fetchers/view models. Every case checks the
// formatted text is built only from fields the API already served — "evidence, never identity"
// (a match/cluster never claims to know what a signal IS). No DOM under node:test.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  clusterLoadErrorText, clusterSummary, fetchCluster, fetchSignatureMatch, signatureMatchSummary,
  type Cluster, type SignatureMatch,
} from "../src/app/explore/signature";

function fakeClient(handlers: { get?: (path: string) => unknown }) {
  return {
    get: async <T>(path: string): Promise<T> => {
      if (!handlers.get) throw new Error(`unexpected GET ${path}`);
      return handlers.get(path) as T;
    },
  };
}

// ---- signature match ----

const match = (over: Partial<SignatureMatch> = {}): SignatureMatch => ({
  schema: 1, emitter_id: "e1", t: 1_789_300_820.5, outcome: "none",
  features_ref: "features:1", signatures_rev: 3, candidates: [], reasons: [],
  ...over,
});

const candidate = (name: string, score: number) => ({
  signature: { id: "sig1", version: 1 }, name, score,
  agreement: [], missing: [], conflicting: [], recipe: null,
});

test("fetchSignatureMatch: GETs /api/signatures/match?emitter=<id>, id encoded", async () => {
  let seen: string | null = null;
  const client = fakeClient({ get: (p) => { seen = p; return { emitter: "e/1", match: null, history: [] }; } });
  const r = await fetchSignatureMatch(client, "e/1");
  assert.equal(seen, "/api/signatures/match?emitter=e%2F1");
  assert.deepEqual(r, { emitter: "e/1", match: null, history: [] });
});

test("signatureMatchSummary: not compared yet, full/partial candidates, 'none' reads as no catalogue match — not unknown", () => {
  assert.equal(signatureMatchSummary(null), "Not compared yet.");
  assert.equal(
    signatureMatchSummary(match({ outcome: "full", candidates: [candidate("RDS-PI", 0.92)] })),
    "Matches RDS-PI — 92% agreement",
  );
  assert.equal(
    signatureMatchSummary(match({ outcome: "partial", candidates: [candidate("P25", 0.55)] })),
    "Possibly P25 — 55% agreement, unconfirmed",
  );
  assert.equal(signatureMatchSummary(match({ outcome: "none", candidates: [] })), "No catalogue match — not the same as unknown.");
});

// ---- clusters ----

const cluster = (over: Partial<Cluster> = {}): Cluster => ({
  id: "cluster:0199abc", state: "active", members: 4, member_ids: ["e1", "e2", "e3", "e4"],
  created_at_s: 1, updated_at_s: 2, observations: 17, suspect_fraction: 0, feature_set_version: "v1",
  merged_into: null, signature: null, centroid: [], events: [],
  ...over,
});

test("fetchCluster: GETs /api/clusters/<id>, id encoded", async () => {
  let seen: string | null = null;
  const client = fakeClient({ get: (p) => { seen = p; return cluster(); } });
  const r = await fetchCluster(client, "cluster:0199abc");
  assert.equal(seen, "/api/clusters/cluster%3A0199abc");
  assert.deepEqual(r, cluster());
});

test("clusterSummary: member/observation counts, singular member handled", () => {
  assert.equal(clusterSummary(cluster({ members: 4, observations: 17 })), "Measures like 4 other emitters seen before (17 observations)");
  assert.equal(clusterSummary(cluster({ members: 1, observations: 3 })), "Measures like 1 other emitter seen before (3 observations)");
});

test("clusterLoadErrorText: wraps the server's own error text", () => {
  assert.equal(clusterLoadErrorText({ code: "not_found", message: "no such cluster" }), "Cluster lookup failed: no such cluster (not_found)");
});
