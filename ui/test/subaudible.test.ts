// T-988 (SIGNAL-090): the Listen header's CTCSS/DCS label. The tone, the code and whether there is
// one at all come from the backend's status record (blind, measured from the discriminator); the
// client only formats them — "no tone" is shown as an answer, and a stream that reports nothing
// (a mode nobody looked at) shows nothing.
import { test } from "node:test";
import assert from "node:assert/strict";
import { type AudioStatus, parseRecord, RECORD_HEADER_LEN, REC_STATUS } from "../src/audio-frames";
import { audioSubText, subaudibleText } from "../src/app/dock/outputs";

const base: AudioStatus = {
  level_dbfs: -20, squelch_open: true, agc_gain_db: 0, frames: 10, squelched_frames: 0,
  lost_samples: 0, latency_ms: 1, backlog_s: 0,
};

test("CTCSS, a second tone, a non-standard tone, DCS with its alias, none and measuring", () => {
  assert.equal(subaudibleText({ ...base, subaudible: "ctcss", ctcss_hz: 131.8, tone_hz: 131.79 }), "CTCSS 131.8 Hz");
  assert.equal(
    subaudibleText({ ...base, subaudible: "ctcss", ctcss_hz: 100, tone_hz: 100.02, tone2_hz: 162.21 }),
    "CTCSS 100.0 Hz + 162.2 Hz",
  );
  assert.equal(subaudibleText({ ...base, subaudible: "tone", tone_hz: 120.04 }), "tone 120.0 Hz (non-standard)");
  assert.equal(
    subaudibleText({ ...base, subaudible: "dcs", dcs_code: "023", dcs_polarity: "normal", dcs_alias: "047I" }),
    "DCS 023N (≡ 047I)",
  );
  assert.equal(subaudibleText({ ...base, subaudible: "none", subaudible_s: 6 }), "no tone");
  assert.equal(subaudibleText({ ...base, subaudible: "measuring", subaudible_s: 0.5 }), "tone: measuring…");
  assert.equal(subaudibleText(base), null, "a WFM/AM stream reports nothing, so nothing is shown");
});

test("the label joins the header line, read from a status record off the wire", () => {
  const json = JSON.stringify({ ...base, subaudible: "dcs", dcs_code: "754", dcs_polarity: "normal", dcs_alias: "116I" });
  const payload = new TextEncoder().encode(json);
  const buf = new ArrayBuffer(RECORD_HEADER_LEN + payload.length);
  const view = new DataView(buf);
  view.setUint8(0, REC_STATUS);
  view.setUint32(4, payload.length, true);
  new Uint8Array(buf, RECORD_HEADER_LEN).set(payload);
  const rec = parseRecord(buf);
  assert.ok(rec && rec.type === "status", JSON.stringify(rec));
  const tone = subaudibleText(rec.status);
  assert.equal(audioSubText("nbfm", 48_000, 1, null, tone), "NBFM audio · 48 kHz · DCS 754N (≡ 116I)");
  assert.equal(audioSubText("nbfm", 48_000), "NBFM audio · 48 kHz", "unchanged without a label");
});
