// T-150 (ADR-0013 §8, §4.8): Outputs dock pure helpers and the `dock/api.ts` contract (id
// generation, dedupe, address formatting; AudioSession itself needs a browser, so it's excluded
// per §6 "Canvas, WebGL and audio are never tested headless").
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createStore } from "../src/app/store";
import { dismissOutputsStrip, initialState, outputsStripShown, upsertOutput, type AppState, type OutputEntry } from "../src/app/state";
import { startRecordsOutput, startListen, stopOutput, type ListenTarget } from "../src/app/dock/api";
import {
  audioSubText, copyAddressText, levelPct, listenQuery, listenTcpTarget, nextId, outputsCountText, recordsTcpTarget, refusalText,
} from "../src/app/dock/outputs";
import type { ControlClient } from "../src/controls/client";
import type { AppContext } from "../src/app/context";

function fakeCtx(): AppContext {
  const store = createStore(initialState());
  const client = { get: async () => ({}), post: async () => ({}), put: async () => ({}), del: async () => ({}) } as unknown as ControlClient;
  return { store, client, token: "t" };
}

test("nextId is stable-prefixed and unique across calls", () => {
  const a = nextId("listen"), b = nextId("listen");
  assert.notEqual(a, b);
  assert.match(a, /^listen\d+$/);
  assert.match(b, /^listen\d+$/);
});

test("listenQuery and listenTcpTarget: emitter vs band, rounded Hz", () => {
  assert.equal(listenQuery({ kind: "emitter", emitterId: "e1", label: "" }), "emitter=e1");
  assert.equal(listenQuery({ kind: "band", fLoHz: 100_300_100.6, fHiHz: 100_400_000, label: "" }), "f_lo=100300101&f_hi=100400000");
  assert.equal(listenTcpTarget({ kind: "emitter", emitterId: "e1", label: "" }), "open/listen?emitter=e1");
});

test("recordsTcpTarget and copyAddressText never include the token", () => {
  assert.equal(recordsTcpTarget("p1", "o1"), "inspector/p1/o1");
  const text = copyAddressText("127.0.0.1:8788", "open/listen?emitter=e1");
  assert.equal(text, "tcp://127.0.0.1:8788 open/listen?emitter=e1");
  assert.doesNotMatch(text, /token/);
});

test("outputsCountText matches the mockup's wording", () => {
  const audio: OutputEntry = { id: "a", kind: "audio", label: "", sub: "", state: "live", tcpTarget: null, muted: false, levelDbfs: null, recordsPerS: null, emitterId: null, pipelineId: null, message: null };
  const rec: OutputEntry = { ...audio, id: "b", kind: "records" };
  assert.equal(outputsCountText([]), "0 live · 0 pipelines");
  assert.equal(outputsCountText([audio]), "1 live · 0 pipelines");
  assert.equal(outputsCountText([audio, rec]), "2 live · 1 pipeline");
  assert.equal(outputsCountText([rec, { ...rec, id: "c" }]), "2 live · 2 pipelines");
});

test("audioSubText, levelPct and refusalText formatting", () => {
  assert.equal(audioSubText("wfm", 48_000), "WFM audio · 48 kHz");
  assert.equal(audioSubText(undefined, undefined), "audio");
  assert.equal(levelPct(null), 6);
  assert.equal(levelPct(-80), 6);
  assert.equal(levelPct(-20), 100);
  assert.equal(levelPct(-50), 50);
  assert.equal(refusalText(403, "restricted"), "refused (403): restricted");
  assert.equal(refusalText(0, "only mono is supported"), "only mono is supported");
});

test("startRecordsOutput: adds one entry, dedupes by pipeline id, Stop removes it", () => {
  const ctx = fakeCtx();
  const id1 = startRecordsOutput(ctx, { pipelineId: "p1", outputId: "o1", label: "RDS" });
  assert.ok(id1);
  assert.equal(ctx.store.get().outputs.length, 1);
  const entry = ctx.store.get().outputs[0];
  assert.equal(entry.kind, "records");
  assert.equal(entry.pipelineId, "p1");
  assert.equal(entry.tcpTarget, "inspector/p1/o1");

  const id2 = startRecordsOutput(ctx, { pipelineId: "p1", outputId: "o1", label: "RDS" });
  assert.equal(id2, id1);
  assert.equal(ctx.store.get().outputs.length, 1, "the same pipeline output twice doesn't duplicate");

  stopOutput(ctx, id1!);
  assert.equal(ctx.store.get().outputs.length, 0);
  stopOutput(ctx, "unknown-id"); // ignored, no throw
});

test("startListen: the same emitter twice returns the existing entry without adding another", () => {
  const ctx = fakeCtx();
  const target: ListenTarget = { kind: "emitter", emitterId: "e1", label: "101.3 MHz" };
  // Seed an existing audio entry for e1 directly (avoids exercising AudioSession/AudioContext,
  // which needs a browser — see the file header).
  const existing: OutputEntry = {
    id: "listen1", kind: "audio", label: "101.3 MHz", sub: "estimating…", state: "opening",
    tcpTarget: listenTcpTarget(target), muted: false, levelDbfs: null, recordsPerS: null,
    emitterId: "e1", pipelineId: null, message: null,
  };
  ctx.store.set((s: AppState) => ({ outputs: [existing] }));
  const id = startListen(ctx, target);
  assert.equal(id, "listen1");
  assert.equal(ctx.store.get().outputs.length, 1, "no duplicate audio entry for the same emitter");
});

// ---- layout (T-994): the dock BAR is retired; the Active-outputs strip reserves nothing ----

test("T-994: no fixed-height Outputs bar — the shell reserves no bottom row, the page carries no dock", () => {
  const html = readFileSync("src/app/index.html", "utf8");
  assert.doesNotMatch(html, /class="dock"/, "the dock element is gone from the page");
  assert.match(html, /<div class="out-strip" data-slot="outputs"[^>]*\bhidden\b/, "the strip that replaced it starts hidden");
  const base = readFileSync("src/app/base.css", "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
  assert.match(base, /\.app \{[^}]*grid-template-rows: 48px minmax\(0,1fr\); \}/, "no 62 px dock row under the views");
  assert.doesNotMatch(base, /\.dock\b/);
  for (const f of ["src/app/chrome/map-layout.css", "src/app/chrome/phone.css"]) {
    assert.doesNotMatch(readFileSync(f, "utf8").replace(/\/\*[\s\S]*?\*\//g, ""), /> \.dock\b/, `${f} still lays out a dock`);
  }
});

test("T-994: the Active-outputs strip floats, hides when empty, scrolls its own row and has no wide min-width", () => {
  const css = readFileSync("src/app/dock/dock.css", "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
  assert.match(css, /\.out-strip \{ position: fixed;/, "floating chrome, never a grid row");
  assert.match(css, /\.out-strip\[hidden\] \{ display: none; \}/, "hidden takes no pixels");
  assert.match(css, /\.out-strip \.outs \{[^}]*overflow-x:\s*auto/);
  const strip = /\.out-strip \{([^}]*)\}/.exec(css)?.[1] ?? "";
  assert.doesNotMatch(strip, /(^|[^-])height:/, "the strip is as tall as its chips — no fixed bar height");
  for (const m of css.matchAll(/min-width:\s*(\d+)px/g)) assert.ok(Number(m[1]) <= 400);
  assert.match(css, /@media \(max-width:\s*900px\)\s*\{[\s\S]*\.out-strip \{[^}]*bottom: calc\(var\(--sheet-bottom, 8px\) \+ 56px/, "above the sheet's peek on a narrow screen");
});

test("T-994: the strip shows only while something is open, and Close holds until the set of outputs changes", () => {
  const ctx = fakeCtx();
  const a: OutputEntry = { id: "a", kind: "audio", label: "", sub: "", state: "live", tcpTarget: null, muted: false, levelDbfs: null, recordsPerS: null, emitterId: "e1", pipelineId: null, message: null };
  assert.equal(outputsStripShown(ctx.store.get()), false, "nothing open: no strip");
  ctx.store.set(upsertOutput(a));
  assert.equal(outputsStripShown(ctx.store.get()), true);
  ctx.store.set(dismissOutputsStrip);
  assert.equal(outputsStripShown(ctx.store.get()), false, "closed");
  ctx.store.set(upsertOutput({ ...a, levelDbfs: -20 }));
  assert.equal(outputsStripShown(ctx.store.get()), false, "a status update on the same output keeps it closed");
  ctx.store.set(upsertOutput({ ...a, id: "b" }));
  assert.equal(outputsStripShown(ctx.store.get()), true, "a NEW output reopens it");
});
