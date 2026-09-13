# C18 · fingerprint-signatures
> Layer C — Characterize · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C13, C14, C15, C16, C10, C21 · Used by: C27, C22, C10, C21, C12, C30, C28

## Purpose
Maintains editable signatures built on the docs/04 §7.6 fingerprint. It matches emission clusters against them and groups unknown emissions into "the same thing I saw before". **C18 owns cross-time emission clustering**; C10 links short-time tracks and C21 aligns messages within a cluster — three scales, one owner each (docs/06 §5). It generalises rtl_433 flex specs beyond OOK and grounds emitter identity in the inventory (C27). It serves workflow step 4 (known vs unknown) and steps 6–7 (a matched signature routes to a decoder pipeline). RF-hardware fingerprinting of individual transmitters is a later hook.

## Interface
- **Input: `EmissionFeatures`** (provisional), aggregated per track or cluster, each with uncertainty:
  - Band, raster, OBW, family, levels, deviation/constellation, symbol rate (C13–C16).
  - Line code, preamble, sync word, packet-length distribution, CRC parameters (C21).
  - Periodicity, TDMA frame, hop set (C10).
  - Optional impairment features: CFO offset, IQ imbalance.
- **Output: `SignatureMatch`.**
  - `candidates[]`: id, version, score, per-field agreement, missing fields.
  - Or `new_cluster_id` with membership confidence.
- **Unknown handling.** Too few fields gives `partial` with ranked candidates, not an identity.
- **`Signature` object:**
  - Per-field tolerances; symbol rate defaults to ±1% (docs/04 §7.6).
  - Required/optional fields.
  - Provenance: user, rtl_433 import, or decoder-confirmed.
  - C22 pipeline binding.
  - Legal tag (`metadata-only: encrypted`, `no-content: common-carrier paging`, docs/04 §1.3).
- **Config:** clustering parameters, normalisation, match thresholds.

## Methods
- **Schema** (docs/04 §7.6): band, raster, OBW, family, levels, deviation/constellation, symbol rate, line code, preamble, sync word, packet lengths, periodicity, TDMA period, CRC parameters, hop set.
- **Discriminating rules** (docs/04 §7.6): symbol rate + deviation + sync word identifies most LMR/ISM protocols; periodicity + length distribution separates sensor families.
- **Matching** (proposal, not from the docs):
  1. Gate on band and family.
  2. Check numeric tolerances.
  3. Match sync words allowing bit errors, in both polarities and all PSK rotations (docs/04 §7.3).
  4. Score, down-weighting missing fields.
- **Clustering unknowns:** DBSCAN on normalised features (docs/04 §7.7); an incremental variant on the device.
- **Seeding:**
  - rtl_433 flex specs: modulation, short/long widths, reset limit, sync, bit count (docs/04 §7.5).
  - The docs/04 §1.2 protocol table.
  - C21 draft decoders, accepted when they pass a CRC check on held-out bursts (docs/04 §7.7).
- **Individual transmitters (later):** ORACLE-style CNNs report ~1.4% error on 16 USRP X310s captured OTA; channel robustness is weak (docs/03 §4.1). Oscillator offset (AWARE-051) is the cheap start.
- **Non-communications signatures** (RFI combs, radar rotation; docs/04 §2 #5, #9) need spectral-shape fields that §7.6 lacks.

## Platform constraints
- **Compute and storage.** Low compute: matching runs per track (docs/06). Storage is SQLite-class, shared with C27.
- **Upstream quality.** Symbol-rate precision depends on burst length. IMD ghosts and images from the 8-bit, preselector-less front end have real-looking parameters (docs/02 §1.7; docs/04 §10.3). Never mint signatures from all-suspect clusters.
- **Clock drift.** Temperature drift confounds raster-offset and oscillator fingerprints until C05 calibrates (docs/04 §10.1).
- **Handheld use.** Position and multipath change between sightings.

## Prior art and reuse
- **rtl_433:** `-X` flex and `-A` analyzer, ~380 devices; the "detect → characterise → auto-decode" model (docs/03 §3.3). Licence: check. Active.
- **URH:** cross-message field inference. GPLv3, archived 2026-03.
- **CRC RevEng, delsum:** CRC parameters. Licence: check.
- **Artemis/sigidwiki:** import as coarse signatures. Licence: check.
- **radiosonde_auto_rx:** type identification. Licence: check.
- **SatDump pipeline registry:** signature → decoder chain (docs/03 §3.6).
- **ORACLE, WiSig datasets:** licence: check.

## Pitfalls
- **Type, not instance.** Identical sensors share a signature; separate instances using decoded IDs, RSSI or location.
- **Tolerances.** Drift (temperature, battery, Doppler) fragments tight clusters; loose tolerances merge protocols. P25 C4FM and DMR both run at 4800 sym/s (DMR derived: 9600 bps, 4 levels) and differ in deviation (±600/±1800 vs ±648/±1944 Hz) and sync word.
- **Partial observation.** Low SNR drops fields; hoppers are only partly seen by a 20 MHz window (docs/04 §4.7).
- **Sync-word ambiguity** from polarity and rotation.
- **DBSCAN** is sensitive to density parameters and scaling.
- **Receiver dependence.** RF fingerprints depend on receiver and channel (AWARE-048).
- **Privacy and legal:**
  - Signatures can enable tracking people (TPMS re-identification, BLE tracking, ID census); keep instance IDs local.
  - Characterise rolling codes without replay.
  - Encrypted traffic gets metadata-only signatures.
- **Imported signatures** are untrusted input; validate them.

## Testing
- **Synthetic populations:**
  - Parameters from docs/04 §1.2/§4.6, including the P25/DMR near-collision, with jitter and missing fields.
  - Metrics: match precision/recall, cluster purity (ARI), fragmentation and merge rates.
  - The only numeric target in the docs is ±1% on symbol rate.
- **SigMF fixtures:**
  - Hours of 315/433.92 MHz sensors with rtl_433 type and ID truth.
  - AIS; RS41.
  - P25 control channel vs DMR.
  - LoRa US915; BLE advertising channels 37/38/39.
  - Imported rtl_433 flex specs must match these captures.
- **Round trip:** C21 draft signature → CRC pass rate on held-out bursts.
- **Live:** multi-day cluster stability; mobile use.

## Example use cases
Regenerated from `use-cases.yaml`:
- AWARE-029 — Power-line arcing locator
- AWARE-030 — Switching-supply / LED / inverter RFI signatures
- AWARE-047 — RF fingerprinting of same-model transmitters
- AWARE-048 — RF fingerprinting robustness across channels
- AWARE-056 — Woodpecker history replay
- RESEARCH-016 — Iridium transmitter fingerprinting (SatIQ)
- RESEARCH-029 — BLE tracking despite MAC randomization
- RESEARCH-062 — RF fingerprinting / physical-layer auth (ORACLE)
- AWARE-035 — (emission clustering)
- RESEARCH-018 — (signature/fingerprint)

## Open questions
- **Clustering owner (resolved, docs/06 §5).** C18 owns cross-time emission clustering; C10 links short-time tracks; C21 aligns messages within a cluster. Three scales, one owner each.
- **Signature format.** rtl_433 flex compatibility, versioning, sharing (ADR). Batch or online clustering?
- **Schema gaps.** §7.6 has no fields for RFI combs or radar PRI/scan, yet most C18-primary use cases are non-protocol (AWARE-029/030/056).
- **Type vs instance.** Signature (type) vs C27 emitter (instance).
- **Privacy.** The mapping makes C18 primary for RESEARCH-029 (BLE tracking despite MAC randomization). Decide the policy for individual-transmitter fingerprinting and tracking.

## Reading list
1. docs/04 §7.6 "Protocol fingerprinting"
2. docs/04 §7.7 "Blind reverse engineering of unknown protocols (practical loop)"
3. docs/04 §7.5 "How existing tools approach it"
4. docs/03 §3.3 "Automatic device and protocol decoders"
5. docs/04 §4.7 "Hop detection and burst timing"
6. docs/03 §4.1 "Datasets and toolkits" (RF fingerprinting)
