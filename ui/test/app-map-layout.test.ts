import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const css = readFileSync("src/app/chrome/map-layout.css", "utf8");
const entry = readFileSync("src/app/app.css", "utf8");

test("MAP-01: canvas fills the viewport and chrome floats over it", () => {
  assert.match(css, /#view-explore > \.centre \{[^}]*inset: 0/);
  assert.match(css, /width: 100vw/);
  // T-993: the top bar does not float over the map any more — it takes no pixels in Explore.
  assert.match(css, /\.app:has\(#view-explore:not\(\[hidden\]\)\) > \.bar \{ display: none; \}/);
  assert.doesNotMatch(css, /> \.bar \{ position: fixed/, "a floating full-width bar is still a bar");
  assert.match(css, /#view-explore > \.side[^{]*\{[^}]*position: absolute/);
  assert.match(css, /overflow: hidden/);
  assert.match(entry, /map-layout\.css/);
});
