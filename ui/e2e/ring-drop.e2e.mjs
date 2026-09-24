// T-845: the drawn IQ horizon never claims IQ the ring has already dropped, across whole-slot drops.
//
// OBSERVED 2026-09-22 (task-deflake-appsurface, 9e08619d): once the backend is older than its
// retention, the IQ ring drops old IQ a whole slot at a time (7.5 s of the fixture's 120 s ring,
// often AHEAD of the retention bound), while the page re-reads the ring window only every
// CAPTURE_CLOCK_MS = 5 s. For up to one poll after a drop the horizon drawn per frame was up to a
// slot older than the oldest sample the ring still held: the canvas claimed IQ that was gone, and a
// clip or demod asked for inside that slice fails.
//
// The fix is the backend stating its next drops (`/api/timeline`'s `window.buffered.drops`) and the
// page honouring each one shortly before its time. This spec runs its own server with a 20 s
// retention, so the ring wraps within seconds and drops a 1.25 s slot several times per poll —
// harder than the fixture's default — and then, sample by sample, holds the horizon the page drew
// against `buffered.t0_s` asked for just before reading it.
//
// COUNT-BASED: the ring is aged by watching `/api/iqbuffer`'s eviction counter, and the assertion
// loop runs a fixed number of samples, each after rendered frames; nothing in the assertion is a
// sleep. The control proves the scenario was reached: in some sample the page's own (5 s old) ring
// snapshot, with the retention bound, is OLDER than the server's oldest sample — the old rule's
// defect — while the horizon actually drawn is not.
import test from "node:test";
import assert from "node:assert/strict";
import { Browser } from "./harness.mjs";
import { startBackend } from "./backend.mjs";

// This file's own backend, at a fixed offset inside ITS LANE's 32-port band (see fog-of-war).
const PORT = Number(process.env.HK_E2E_PORT ?? 8791) + 20;
const SAMPLES = 120;

test("T-845: the drawn IQ horizon is never older than the ring's oldest sample, across ring drops", async (t) => {
  const backend = await startBackend({ port: PORT, args: ["--iq-retention", "20s"] });
  t.after(() => backend.stop());
  const api = async (route) => {
    const r = await fetch(`${backend.origin}${route}`, { headers: { authorization: `Bearer ${backend.token}` } });
    assert.equal(r.status, 200, `${route}: ${r.status}`);
    return r.json();
  };

  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();
  assert.equal(await page.goto(`${backend.origin}/#token=${backend.token}`), "load");
  try {
    await page.waitFor("the canvas to draw and the ring readout to hold an IQ horizon",
      `!!document.querySelector('.sf-canvas') && !!document.querySelector('.sf-ring')?.dataset.iqS`,
      // A fresh backend's first mount waits on its own first ingest: once measured over 60 s
      // beside another lane's server, so this wait is generous — it is setup, not the claim.
      { timeoutMs: 120000 });
  } catch (e) {
    // Say what the page DID show, and what the server said, rather than only "timed out".
    const seen = await page.eval(`(() => ({ url: location.href, canvas: !!document.querySelector('.sf-canvas'),
      ring: document.querySelector('.sf-ring')?.textContent ?? null, note: document.querySelector('.sf-note')?.textContent ?? null,
      body: document.body?.innerText?.slice(0, 400) }))()`).catch((x) => String(x));
    t.diagnostic(`page: ${JSON.stringify(seen)}`);
    t.diagnostic(`server window: ${JSON.stringify(await api("/api/timeline?columns=1&rows=1").catch((x) => String(x)))}`);
    t.diagnostic(`backend log tail: ${backend.log().slice(-1500)}`);
    throw e;
  }

  // Age the backend past its retention: the ring has wrapped and dropped whole slots. Bounded by a
  // count of polls, each one a question to the server, not a sleep standing in for a condition.
  let status = null;
  for (let i = 0; i < 400; i++) {
    status = await api("/api/iqbuffer?limit=1");
    if (status.evicted?.chunks >= 3) break;
    await page.frames(10);
  }
  t.diagnostic(`ring: ${status.slot_count} slots of ${status.chunk_bytes} B, retention ${status.retention_s} s, ` +
    `evicted ${status.evicted?.chunks} slots, holds ${status.span_s?.toFixed(2)} s`);
  assert.ok(status.evicted.chunks >= 3, `the ring never wrapped: ${JSON.stringify(status.evicted)}`);

  const read = () => page.eval(`(() => { const d = document.querySelector('.sf-ring').dataset;
    return { iq: +d.iqS, ret: +d.retentionS, edge: +d.edgeS, ringT0: d.ringT0S ? +d.ringT0S : null,
      dropT0: d.dropT0S ? +d.dropT0S : null }; })()`);
  const serverT0s = new Set();
  let staleWouldClaim = 0, worst = Infinity;
  for (let i = 0; i < SAMPLES; i++) {
    // The server's oldest sample FIRST, then what the page has drawn: the page's frame is at least
    // as late as that answer (to within a frame), so the ring can only have dropped more by then.
    const w = (await api("/api/timeline?columns=1&rows=1")).window;
    const st = await read();
    assert.ok(w.buffered && Number.isFinite(st.iq), `sample ${i}: no ring window or no horizon: ${JSON.stringify({ w, st })}`);
    const t0 = w.buffered.t0_s;
    serverT0s.add(t0);
    worst = Math.min(worst, st.iq - t0);
    assert.ok(st.iq >= t0 - 1e-6,
      `sample ${i}: the drawn IQ horizon ${st.iq} claims ${(t0 - st.iq).toFixed(3)} s of IQ the ring dropped ` +
      `(server buffered.t0_s ${t0}; page drew from ring t0 ${st.ringT0}, retention bound ${st.ret}, drop ${st.dropT0})`);
    // The horizon is exactly the newer of what the page drew from: the polled ring, the retention
    // bound, and the scheduled drop it applied — no other source.
    assert.ok(Math.abs(st.iq - Math.max(st.ringT0, st.ret, st.dropT0 ?? -Infinity)) < 1e-6,
      `sample ${i}: horizon ${st.iq} is not the newest of ${JSON.stringify(st)}`);
    if (Math.max(st.ringT0, st.ret) < t0 - 1e-6) staleWouldClaim++;
    await page.frames(3);
  }
  t.diagnostic(`${SAMPLES} samples over ${serverT0s.size} distinct ring t0 values; the pre-T-845 rule would have ` +
    `claimed dropped IQ in ${staleWouldClaim}; closest the horizon came to the server's oldest: ${worst.toFixed(3)} s`);
  assert.ok(serverT0s.size >= 3, `the samples must span ring drops: only ${serverT0s.size} distinct t0 values`);
  assert.ok(staleWouldClaim > 0, "the control never fired: no sample had a stale ring snapshot behind a drop, so this proves nothing");
});
