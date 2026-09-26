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
  // T-997: nothing floats at the LEFT edge any more — the inventory lists are sheet content and
  // their counts are pills in the chrome cluster, so `.side` has no screen-space rule here at all.
  assert.doesNotMatch(css.replace(/\/\*[\s\S]*?\*\//g, ""), /#view-explore > \.side/);
  // The sheet is the floating panel over the canvas (`sheet.css`), fixed and above the chrome.
  assert.match(readFileSync("src/app/chrome/sheet.css", "utf8"), /\.sheet \{ position: fixed; z-index: 6;/);
  assert.match(css, /overflow: hidden/);
  assert.match(entry, /map-layout\.css/);
});
