// Signature-match and cluster view models (T-207, ADR-0016 §5, docs/api.md `GET
// /api/signatures/match`, `GET /api/clusters/{id}`). Both are ranked evidence about how an
// emitter *measures*, never an identity: a match never sets `known_status` or a family, and a
// cluster is a type above emitters ("the same thing I saw before"), not what the thing IS. This
// module only fetches and formats what the backend already decided — no scoring or matching runs
// here.
import { apiErrorText } from "./format";

export interface ApiClient { get<T>(path: string): Promise<T> }

// ---- signature match ----

export interface SignatureRef { id: string; version: number }
export interface RecipeRef { id: string; version: number }

export interface FieldAgreement { field: string; measured: unknown; expected: unknown; z: number; ok: boolean }

export interface SignatureCandidate {
  signature: SignatureRef; name: string; score: number;
  agreement: FieldAgreement[]; missing: string[]; conflicting: string[]; recipe: RecipeRef | null;
}

export type MatchOutcome = "full" | "partial" | "none";

export interface SignatureMatch {
  schema: number; emitter_id: string; t: number; outcome: MatchOutcome;
  features_ref: string | null; signatures_rev: number;
  candidates: SignatureCandidate[]; reasons: string[];
}

export interface SignatureMatchResponse { emitter: string; match: SignatureMatch | null; history: SignatureMatch[] }

/** `GET /api/signatures/match?emitter=<id>` (docs/api.md). */
export async function fetchSignatureMatch(client: ApiClient, emitterId: string): Promise<SignatureMatchResponse> {
  return client.get<SignatureMatchResponse>(`/api/signatures/match?emitter=${encodeURIComponent(emitterId)}`);
}

/** The focus panel's one-line signature-match summary. `full`/`partial` name the best candidate
 * and its score; `none` reads as "the catalogue has nothing to say", never as "unknown" (docs/api.md
 * "`none` means the catalogue has nothing to say — not that the emission is unknown"); `null`
 * (nothing computed yet) reads as not-yet-compared, distinct from both. */
export function signatureMatchSummary(m: SignatureMatch | null): string {
  if (!m) return "Not compared yet.";
  const top = m.candidates[0];
  if (top && m.outcome === "full") return `Matches ${top.name} — ${Math.round(top.score * 100)}% agreement`;
  if (top && m.outcome === "partial") return `Possibly ${top.name} — ${Math.round(top.score * 100)}% agreement, unconfirmed`;
  return "No catalogue match — not the same as unknown.";
}

// ---- clusters ----

export interface ClusterCentroidField {
  field: string; kind: "num" | "bits" | "text"; value: unknown;
  sigma: number; spread: number; agreement: number; n: number; method: string;
}
export interface ClusterEvent { kind: string; other_cluster_id: string | null; t_s: number; detail: unknown }

export interface Cluster {
  id: string; state: "pending" | "active" | "merged" | "promoted";
  members: number; member_ids: string[]; created_at_s: number; updated_at_s: number;
  observations: number; suspect_fraction: number; feature_set_version: string;
  merged_into: string | null; signature: SignatureRef | null;
  centroid: ClusterCentroidField[]; events: ClusterEvent[];
}

/** `GET /api/clusters/{id}` (docs/api.md); `id` may carry the `cluster:` prefix or not — the
 * route accepts either. */
export async function fetchCluster(client: ApiClient, id: string): Promise<Cluster> {
  return client.get<Cluster>(`/api/clusters/${encodeURIComponent(id)}`);
}

/** The focus panel's cluster line: "the same thing seen before" (ADR-0016 §5) — a type above
 * emitters, never an identity, so this never claims to say what the signal IS. */
export function clusterSummary(c: Cluster): string {
  return `Measures like ${c.members} other emitter${c.members === 1 ? "" : "s"} seen before (${c.observations} observations)`;
}

/** A cluster fetch failed (e.g. it merged away and the id no longer resolves, or went back to
 * `pending`) — reported plainly, not as "no cluster". */
export function clusterLoadErrorText(e: unknown): string {
  return `Cluster lookup failed: ${apiErrorText(e)}`;
}
