// T-811 (MAP-11): artifact-of relations drawn as links to their source (AWARE-042, geometric tier).
import { test } from "node:test";
import assert from "node:assert/strict";
import { artifactLinkQuads, artifactLinks, type LinkRow } from "../src/surface/artifacts";
import { composeOverlays, defaultPaneLayers, withLayer, type OverlayLayerFn } from "../src/surface/layers";
import type { PaneView } from "../src/surface/surface";

const iv = { t_start_s: 0, t_end_s: 10, open: false };
const row = (id: string, f: number, rel: LinkRow["relation"] = null): LinkRow =>
  ({ id, state: "candidate", f_lo_hz: f - 1e3, f_hi_hz: f + 1e3, presence: { last_interval: iv }, relation: rel });
const box = { f0Hz: 90e6, f1Hz: 110e6, t0Ns: 0, t1Ns: 10e9 };
const rect = { x: 0, y: 0, w: 800, h: 400 };

test("MAP-11: artifact-of draws a link to its source; other relations and missing sources draw none", () => {
  const rows = [
    row("src", 100e6),
    row("img", 95e6, { kind: "artifact-of", artifact: "image", source_id: "src" }),
    row("dup", 96e6, { kind: "duplicate-of", source_id: "src" }),
    row("orphan", 97e6, { kind: "artifact-of", artifact: "harmonic", source_id: "gone" }),
  ];
  const links = artifactLinks(rows, 10e9);
  assert.equal(links.length, 1);
  assert.equal(links[0].id, "img");
  assert.equal(links[0].sourceId, "src");
  const q = artifactLinkQuads(links, box, rect);
  assert.ok(q.length > 3 && q.every((x) => x.kind === "artifact-link" && x.id === "img"));
});

test("MAP-11: the layer is off by default and toggling it reaches the overlay hook only when visible", () => {
  const fn: OverlayLayerFn = () => artifactLinkQuads(artifactLinks([
    row("src", 100e6), row("img", 95e6, { kind: "artifact-of", artifact: "image", source_id: "src" })], 10e9), box, rect);
  const pane = {} as unknown as PaneView;
  const off = defaultPaneLayers("p");
  assert.equal(composeOverlays(off, { artifacts: fn }, pane, 0).length, 0);
  const on = withLayer(off, "artifacts", true);
  assert.ok(composeOverlays(on, { artifacts: fn }, pane, 0).length > 0);
});
