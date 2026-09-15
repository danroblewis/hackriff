// T-061 output recordings: Demod/Record hook dispatch, progress and stop flow, download links.
import { test } from "node:test";
import assert from "node:assert/strict";
import { ControlError } from "../src/controls/client";
import { DEMOD_KINDS, OutputTracker, type OutputSession, RECORD_KINDS, downloadHref, progressText, startOutputs } from "../src/outputs";
import { SelectionStore, demodHook, recordSelection, runSelectionAction } from "../src/selections";

const session = (over: Partial<OutputSession> = {}): OutputSession => ({
  id: "0190-out", active: true, selection_id: "sel-1", emitter_id: null, f_lo_hz: 433.8e6, f_hi_hz: 434.1e6,
  kinds: ["bits", "symbols", "audio"], max_s: 60, max_bytes: 1 << 30, bytes: 2048, started_at: "2026-09-14T00:00:00Z",
  elapsed_s: 1.5, ended: null, links_saved: 0,
  files: [{ kind: "bits", file: "bits.ru8", sidecar: "bits.json", state: "recording", bytes: 2048, records: 12, dropped_records: 0,
    message: null, recording_id: null, bitstream_id: null, url: "/api/outputs/0190-out/files/bits.ru8",
    sidecar_url: "/api/outputs/0190-out/files/bits.json", extra_urls: [] }],
  ...over,
});

function fakeClient(responses: { get?: () => unknown; post?: (p: string, b: unknown) => unknown }) {
  const calls: [string, string, unknown][] = [];
  return {
    calls,
    get: async <T,>(p: string) => { calls.push(["GET", p, undefined]); return responses.get!() as T; },
    post: async <T,>(p: string, b?: unknown) => { calls.push(["POST", p, b]); return responses.post!(p, b) as T; },
  };
}

function store() {
  let id = 0;
  return new SelectionStore({ newId: () => `sel-${++id}`, now: () => 1, backend: null });
}

test("demod hook: Listen starts inside the gesture, then bits/symbols/audio are recorded and linked", async () => {
  const s = store();
  const sel = s.add({ f_lo: 433.8e6, f_hi: 434.1e6, name: "sensor" });
  const log: string[] = [];
  const client = fakeClient({ post: (p, b) => { log.push(`post ${p}`); return { recording: session({ kinds: (b as { kinds: string[] }).kinds }) }; } });
  const tracked: OutputSession[] = [];
  const hooks = {
    inspect: async () => ({ status: "done" as const, message: "" }),
    listen: () => undefined,
    demod: demodHook(client, (x) => log.push(`listen ${x.name}`), { tracker: { track: (o) => tracked.push(o) } }),
    record: (x: typeof sel) => recordSelection(client, x, { tracker: { track: (o) => tracked.push(o) } }),
  };
  const p = runSelectionAction("demod", [sel], s, hooks);
  assert.equal(log[0], "listen sensor", "listen starts synchronously in the click, before the request");
  const [d] = await p;
  assert.equal(d.status, "done");
  assert.deepEqual(client.calls[0], ["POST", "/api/outputs/record/start", { selection_id: sel.id, kinds: [...DEMOD_KINDS] }]);
  assert.deepEqual(s.get(sel.id)!.links.map((l) => `${l.kind}:${l.target}:${l.note}`), ["demodulation:output:0190-out:bits,symbols,audio"]);
  assert.equal(tracked.length, 1);

  const [r] = await runSelectionAction("record", [sel], s, hooks);
  assert.equal(r.status, "done");
  assert.deepEqual(client.calls[1][2], { selection_id: sel.id, kinds: [...RECORD_KINDS] });
  assert.equal(s.get(sel.id)!.links.at(-1)!.kind, "recording");
  assert.equal(log.length, 3, "record does not start Listen");

  const noAudio = demodHook(client, () => log.push("listen"), { kinds: ["bits"] });
  await noAudio(sel);
  assert.equal(log.filter((l) => l === "listen").length, 0, "no Listen without audio");
});

test("start refusals: quota and busy are failed outcomes with the server's reason", async () => {
  const sel = { id: "s", name: "FM", f_lo: 1, f_hi: 2, tags: [], links: [], created: 1, updated: 1 };
  const quota = fakeClient({ post: () => { throw new ControlError(507, "quota", "output quota full: 9 of 8 bytes used"); } });
  const { outcome, session: none } = await startOutputs(quota, sel, ["iq"]);
  assert.equal(none, null);
  assert.equal(outcome.status, "failed");
  assert.match(outcome.message, /record "FM": quota: output quota full/);
});

test("tracker: polls while active, reports finish once, stop finalises; progress and links", async () => {
  let current = session();
  const finished: string[] = [];
  const changes: number[] = [];
  const scheduled: (() => void)[] = [];
  const client = fakeClient({
    get: () => ({ recordings: [current, session({ id: "other", active: true })] }),
    post: (_p, b) => { assert.deepEqual(b, { id: "0190-out" }); current = session({ active: false, ended: "stopped", links_saved: 2, files: current.files.map((f) => ({ ...f, state: "done", bitstream_id: "bs-1" })) }); return { recording: current }; },
  });
  const t = new OutputTracker(client, { onChange: (l) => changes.push(l.length), onFinished: (s) => finished.push(s.id), schedule: (fn) => scheduled.push(fn) });
  t.track(current);
  assert.equal(scheduled.length, 1, "a poll is scheduled while active");
  assert.match(progressText(t.list()[0]), /^recording 1\.5 \/ 60 s · 2\.0 kB of 1\.0 GB · bits recording 12$/);
  current = session({ elapsed_s: 3, bytes: 4096 });
  await t.poll();
  assert.equal(t.list().length, 1, "only sessions started here are tracked");
  assert.equal(t.list()[0].bytes, 4096);
  assert.deepEqual(finished, []);
  assert.equal(await t.stop("0190-out"), null);
  assert.deepEqual(finished, ["0190-out"]);
  assert.match(progressText(t.list()[0]), /^finished \(stopped\) · 2\.0 kB · 2 links saved · bits done 12$/);
  await t.poll();
  assert.deepEqual(finished, ["0190-out"], "finish reported once");
  assert.ok(changes.length >= 3);
  assert.equal(downloadHref("/api/outputs/a/files/bits.ru8", "t k"), "/api/outputs/a/files/bits.ru8?token=t%20k");

  const failing = new OutputTracker(fakeClient({ post: () => { throw new ControlError(404, "not_found", "no output recording x"); } }), { onChange: () => {} });
  assert.equal(await failing.stop("x"), "not_found: no output recording x");
});
