// T-1042: the client's feature flags — one mechanism, off by default.
//
// The claim that matters is the **default**: a flag whose absence is not off is not a flag, it is a
// cutover. `live-ring` ships off, so an unflagged page is the pre-LSR-1 client exactly.
import test from "node:test";
import assert from "node:assert/strict";
import { flagsOf } from "../src/flags";
import { withoutToken } from "../src/app/net";

test("T-1042: every flag is off by default", () => {
  assert.deepEqual(flagsOf(""), { liveRing: false });
  assert.deepEqual(flagsOf("?"), { liveRing: false });
  assert.deepEqual(flagsOf("?token=abc&pane=p1"), { liveRing: false });
});

test("T-1042: `live-ring` is on for the spellings a human or a harness would use", () => {
  for (const q of ["?live-ring=1", "live-ring=1", "?live-ring", "?live-ring=true", "?live-ring=ON", "?a=b&live-ring=yes"]) {
    assert.equal(flagsOf(q).liveRing, true, q);
  }
});

test("T-1042: an explicit off, or a value that is not a yes, is off", () => {
  for (const q of ["?live-ring=0", "?live-ring=false", "?live-ring=no", "?live-ring=off", "?live-ring=maybe", "?liveRing=1"]) {
    assert.equal(flagsOf(q).liveRing, false, q);
  }
});

// ——— the flag has to SURVIVE the boot ———

test("T-1042: stripping the token from the address bar keeps the flags", () => {
  // `takeToken` rewrites the address to drop the credential. It used to rewrite to the PATH, which
  // threw the flags away with it — the flag then did nothing, silently, which is the defect this
  // pins: `?live-ring=1` must still be readable by `flags()` after the boot.
  assert.equal(withoutToken({ pathname: "/", search: "?token=abc" }), "/");
  assert.equal(withoutToken({ pathname: "/", search: "?live-ring=1" }), "/?live-ring=1");
  assert.equal(withoutToken({ pathname: "/", search: "?token=abc&live-ring=1" }), "/?live-ring=1");
  assert.equal(flagsOf(new URL(`http://x${withoutToken({ pathname: "/", search: "?token=abc&live-ring=1" })}`).search).liveRing, true);
  assert.equal(withoutToken({ pathname: "/app.html", search: "" }), "/app.html");
});
