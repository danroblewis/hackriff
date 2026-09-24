import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const css = readFileSync("src/app/chrome/map-layout.css", "utf8");
const entry = readFileSync("src/app/app.css", "utf8");

test("MAP-01: canvas fills the viewport and chrome floats over it", () => {
  assert.match(css, /#view-explore > \.centre \{[^}]*inset: 0/);
  assert.match(css, /width: 100vw/);
  assert.match(css, /> \.bar \{ position: fixed/);
  assert.match(css, /#view-explore > \.side[^{]*\{[^}]*position: absolute/);
  assert.match(css, /overflow: hidden/);
  assert.match(entry, /map-layout\.css/);
});
