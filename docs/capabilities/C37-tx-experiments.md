# C37 · tx-experiments
> Layer G — Specialised · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C01, C04, C06, C25, C28 · Used by: none directly (paired RX via C13, C20, C33, C34)

## Purpose
The **authorised-only** transmit path:
- beacons, sounders, test tones and own-link modems;
- replay of the user's **own** captures to their **own** devices;
- scheduled TX windows with paired receive for propagation, link and lab experiments.

The device is **receive-only by default**. TX needs explicit enablement tied to a licence, rule authority, or an own-device conducted/shielded setup. No jamming, no replay of others' signals, no attack tooling. It supports science and own-device research, not the core survey workflow.

## Interface
- **Inputs (provisional objects):**
  - `TxAuthority`: kind (Part 97 callsign/class, licensed-by-rule, Part 15, own-device conducted), band allowlist, max EIRP per band, expiry, user attestation.
  - `TxPlan`: frequency, bandwidth, power, duty cycle, max duration, waveform source (generator spec, or SigMF capture with `origin: own` provenance), schedule window, paired `RxPlan`.
- **Outputs:** append-only `TxSessionLog` (authority id, actual frequency/gain, start/stop from C06 time, waveform hash, operator confirmation) and timing marks for paired RX.
- **Config:**
  - `tx.enabled: false` shipped default.
  - Enabling needs an authority profile plus per-session UI confirmation.
  - Allowlist and power cap enforced in the core, not only the UI.
  - Always-visible TX indicator; one-press abort.
- **Rates:** HackRF TX ≤20 Msps, 8-bit. Pro adds 1–32× TX interpolation (`docs/01 §1.7`).

## Methods
- **Scheduler lease (C04):**
  - TX is an exclusive, scheduled radio lease.
  - RX is suspended cleanly and marked "TX gap" in provenance so C09/C12 don't log the hole as an event.
  - A single `TxGate` checks authority, allowlist, power cap and duration. Reject by default.
- **Waveforms:** WSPR/MSK beacons (PROP-003, PROP-029), PN or chirp sounders (PROP-014), CW for lab S21 (RESEARCH-051), CSS modems for own links (RESEARCH-063).
- **Replay:**
  - Only own-origin captures (labelled via C28).
  - Refuse when C22/C27 decoded a third-party identity: callsign, ICAO, MMSI, or a keyfob serial not registered as owned (suggestion).
- **Own encrypted links:** keys the user holds; decrypt only those (`docs/04 §1.3`).
- **Paired RX:** precise TX timestamps. Echoes go to C34 (Doppler), C33 (power) and C13/C20 (link quality).

## Platform constraints
- **TX power** (`docs/01 §1.2`):
  - 5–15 dBm below 2.17 GHz.
  - 13–15 dBm at 2.17–2.74 GHz.
  - 0–5 dBm at 2.74–4 GHz.
  - −10 to 0 dBm at 4–6 GHz.

  Beyond short range, an external PA and filter are needed.
- **Half-duplex** (`docs/01 §5`): overlapping radar or monostatic sounding is `needs-other-sdr`. TX→RX switch latency isn't in the docs; measure it.
- **RX damage limit:** −5 dBm (`docs/01 §1.2`). Loopback at +15 dBm needs >20 dB attenuation; use ≥40 dB for margin (arithmetic plus estimate).
- **No hardware TX-disable on One:** gating is software-only, so put the gate in the C01 driver wrapper. HackRF Pro has hardware TX-disable (`docs/01 §1.7`).
- **Spectral purity:** no tunable preselection (`docs/01 §1.3`). Harmonic and spurious levels are undocumented; measure before any antenna use.
- **Battery:** TX draw is undocumented. Budget with `docs/02 §7.2`.

## Prior art and reuse
- **Mayhem TX apps** (`docs/01 §3.3`):
  - Take the one-touch replay UX and the simple capture format.
  - **Do not inherit:** Jammer, OOK Brute, KeeLoq/Security+ TX, BLESpam, GPS Sim, P25 TX. GPL; licence: check.
- **`hackrf_transfer`** (`docs/01 §1.6`): raw 8-bit IQ TX reference.
- **SDRangel Tx plugins and PER tester** (`docs/03 §2.2`): modulator references. Licence: check.
- **Use-case refs:** WsprryPi (PROP-003), jvierine/ionosonde (PROP-014), gr-lora_sdr (RESEARCH-063), LibreVNA (RESEARCH-051). Licence: check.
- **URH simulator** (`docs/03 §3.4`): archived. Own-device test ideas only.

## Pitfalls
- **"Replay" creep:** replaying others' keyfob, garage or pager signals is out of scope. Provenance must be enforced, not advisory.
- **Jamming inside research use cases:** RESEARCH-027 (RollJam: jam + record + replay) is now **`out-of-scope`** (docs/06 §5, §4.4). Capture-only variants (record and replay without jamming, e.g. SIGNAL-047, RESEARCH-028 RollBack) stay in scope.
- **Allowlists vary by country and licence class:** ship none enabled.
- **Always-denied bands:** hard-deny public safety, aviation, GNSS and cellular regardless of profile (`docs/04 §1.3`).
- **Killing your own front end:** TX into an unattenuated receiver, or the amp left on in loopback.
- **Unflagged TX gaps:** they corrupt C12 baselines and look like anomalies.
- **Unfiltered harmonics** of a 15 dBm carrier can land in protected bands.

## Testing
- **Gate unit tests (highest priority):**
  - Default config makes zero TX calls.
  - Out-of-allowlist, over-power, expired and hard-denied requests are rejected.
  - Every accepted request is audited.
  - Abort stops TX within a time bound.
- **Waveforms:** generator → file sink → C01 file replay → C13/C20. Assert frequency, bandwidth, symbol rate and payload against reference decoders (WSPR, CSS).
- **Replay provenance:** a SigMF fixture tagged third-party is refused; one tagged own-origin is accepted.
- **Scheduler:** a TX window yields a "TX gap" span with no detections inside it.
- **Live hardware:** conducted loopback via ≥40 dB attenuation or dummy load in a shielded box; switch latency; harmonics measured on an analyzer.

## Example use cases
Regenerated from `use-cases.yaml` (RESEARCH-027 jam+replay is excluded — `out-of-scope`, docs/06 §5):
- PROP-003 — Own WSPR beacon reach test
- PROP-014 — Build your own coded ionosonde
- PROP-015 — Oblique sounding between two of your own stations
- PROP-016 — NVIS link characterization
- PROP-029 — Meteor-burst link experiment
- PROP-050 — Own 2.4/5.8 GHz rain and foliage link
- PROP-078 — LoRa terrain link-budget mapping
- RESEARCH-051 — Two-port VNA measurements
- RESEARCH-063 — Build a LoRa-like CSS modem
- RESEARCH-036 — ZigBee/802.15.4 security (KillerBee), own network only

## Open questions
- **Build-time exclusion:** should TX also be a build-time feature flag? This is a product ADR.
- **Proving own-origin:** is attestation enough, or should captures be tied to a registered own-device fingerprint (C18)?
- **C06 dependency:** docs/06 C37 omits C06, but sounders and WSPR need accurate time.
- **Attack-framed use cases (resolved, docs/06 §5):** RESEARCH-027 (jamming) is `out-of-scope`; RESEARCH-028/032/038 stay as capture/replay-only or "study published findings, RX-only".
- **Full duplex or other SDRs:** many `tx: required` items (PROP-061–069, RESEARCH-020–026) need them or a testbed. Confirm `needs-other-sdr`.
- **External PA:** is a PA accessory in scope for HF licensed work?

## Reading list
1. `docs/01 §1.2 "Specifications"` — TX power, RX limit, half-duplex.
2. `docs/01 §1.7` (HackRF Pro) — hardware TX-disable, interpolation.
3. `docs/05 §2 "Active SDR radar & sensing with your own transmitter"`; `docs/05 §5 "Wireless device security research (own devices)"`.
5. `docs/01 §3.3 "Apps, external apps (.ppma), and the catalog"` — what not to inherit.
6. `docs/06 §3 "Mapping rules for `use-cases.yaml`"` — `needs-tx` versus `out-of-scope`.
