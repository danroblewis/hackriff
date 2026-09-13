# C22 · decoder-plugins
> Layer D — Demodulate & decode · Status: draft (taxonomy draft 2026-09-13) · Depends on: C11, C15, C17, C19, C20, C21 · Used by: C23, C24, C27, C28, C15, C30, C35

## Purpose
Hosts proven third-party decoders (rtl_433, readsb, AIS-catcher, multimon-ng, SatDump, …) as isolated plugins behind one contract, and routes the right channel to the right decoder from classification. CRC-valid decodes become ground-truth labels for the inventory and classifier. This delivers "decoders are pluggable consumers" (step 7) and verifiable decode (step 6) without rewriting decoders.

## Interface
- **In:** one of
  - `ChannelStream` IQ (rate, centre, sample format);
  - `AudioStream` (C19);
  - `SymbolStream`/`BitStream` (C20);
  - or a SigMF file for offline re-processing.

  All inputs carry the emission id and sample time.
- **Routing input:** `Classification` (C15), `KnownSignalCandidates` (C17), signature match (C18) or `DraftSignature` (C21).
- **Out** (provisional):
  - `DecodedMessage`: plugin id and version, protocol, sample-time timestamp, emission id, CRC/validity flag, JSON fields, restricted-content flag.
  - `Annotation`: SigMF-style time×frequency box with label.
  - `Product`: files, such as SatDump images.
  - `PluginHealth`: up/down, restarts, CPU, backlog.
- **Pipeline registry** (provisional): `signal type → chain → products`, declarative in the SatDump style. It can enter at IQ, audio, soft symbols or bits (docs/04 §7.5).
- **Plugin manifest** (provisional): input kinds and rates, command, output parser, licence, legal class (e.g. "paging: metadata only").

## Methods
- **Contract:** IQEngine-style. IQ in; IQ, audio, bytes or SigMF annotations out. Same for live and recorded input (docs/03 §3.4, §5.2).
- **Wrapping:** subprocesses over pipes or local sockets; parse structured output (rtl_433 JSON); supervise with restart/backoff (docs/03 §7).
- **Routing:** classification plus priors select a registry entry. **Trial decoding** is the final arbiter ("if rtl_433 or DSD locks, that's the ground truth"; docs/03 §7). Ambiguous snippets can run several candidates; keep the CRC-valid result.
- **Background decoding:** always-on schedules at low priority, pre-empted by interactive use (OpenWebRX+; docs/03 §2.4).
- **Feedback:** CRC-valid decodes go to C27/C28 as labels and feed C15 fine-tuning (docs/04 §12 #12).
- **Legal gating:**
  - Common-carrier paging and cellular contents are off-limits even when decodable (docs/04 §1.3). The host drops content fields that manifests mark metadata-only.
  - Encrypted payloads are flagged, never decrypted, except the user's own traffic with the user's keys (e.g. SIGNAL-051).

## Platform constraints
- Typically a few % of a core per decoder (docs/06 §2). Continuous decoders and SatDump pipelines are heaviest; budget against the Orin Nano's 6 A78AE cores (docs/02 §3.3).
- Plugins only see the single ≤20 MHz half-duplex window. ADS-B (1090 MHz) and ISM (433 MHz) can't run at once, so C04 arbitrates decoder demand.
- A subprocess per decoder costs RAM and IPC copies. Feed channel-rate IQ; full-rate is 40 MB/s (docs/02 §3.1).
- Decoders expect specific IQ formats and audio rates. The host converts.

## Prior art and reuse
Status from docs/03 §3.3, §3.5 and §3.6. **Licences: check** for all; the docs don't state them.

| Decoder | Covers | Status |
|---|---|---|
| **rtl_433** | ~380 ISM protocols, JSON/MQTT | active |
| **readsb / dump1090** | ADS-B | active |
| **AIS-catcher** | AIS | v0.70 |
| **multimon-ng** | POCSAG/FLEX/EAS/AFSK from audio | active |
| **Dire Wolf** | APRS | active |
| **radiosonde_auto_rx** | full detect→decode pipeline | active |
| **SatDump CLI** | registry model | 2.0 alpha, very active |
| **gr-satellites** | cubesats | active |
| **dsd-fme** | digital voice | active |

- docs/06 also lists acarsdec/dumpvdl2, dump978, redsea, nrsc5, GNSS-SDR.
- **Mayhem apps:** a backlog and test matrix. GPL (docs/01 §7.1).
- **SDRangel channel plugins:** an alternative host (docs/03 §2.2).
- **GPL isolation:** separate processes over pipes or sockets keep the core's licence options open. The decision is deferred (CLAUDE.md).

## Pitfalls
- **Crashes and hangs:** supervisor plus watchdog on output silence; a dead plugin must not stall the pipeline.
- **Backpressure:** a slow plugin must not block C11. Use bounded queues, drop counters, gap marks.
- **Wall-clock timestamps** from plugins: re-stamp from host sample indices.
- Output parsing breaks across versions. Pin versions and record them in provenance.
- Duplicates from overlapping channels or parallel trials: dedupe by CRC and time.
- Non-CRC-checked "decodes" polluting C28 labels.
- **Legal leakage:** multimon-ng prints pager text by default. Filter in the host.

## Testing
- **Contract tests:** replay SigMF fixtures and compare `DecodedMessage` JSON to golden output.
  - ADS-B (readsb), AIS (AIS-catcher), ISM sensors (rtl_433);
  - NOAA Weather Radio SAME (multimon-ng via C19);
  - RDS (redsea);
  - POCSAG: assert address/timing metadata is present and message text is absent.
- **Routing:** injected classifications select the expected chain. Two-candidate trials keep the CRC-valid result.
- **Fault injection:** kill or hang a plugin. Assert restart, a health event, no upstream stall, bounded memory.
- **Load (Jetson bench):** N concurrent plugins; CPU, latency, drops.

## Example use cases
Provisional until docs/06 §3 mapping:
- SIGNAL-001 — ADS-B / Mode S (baseline)
- SIGNAL-015 — AIS (baseline)
- SIGNAL-052 — rtl_433 long tail
- SIGNAL-074 — Radiosondes (baseline)
- SIGNAL-003 — VHF ACARS
- SIGNAL-008 — All-datalink aggregation
- SIGNAL-067 — NOAA Weather Radio SAME/EAS
- AWARE-070 — IoT sensor population census
- AWARE-062 — Radiosonde launch correlation

## Open questions
- **IPC and framing:** pipes, sockets or the C24 transport? If C24, C22 depends on C24.
- Registry and manifest schema: adopt SatDump's pipeline format or our own?
- **Own-key decryption** (SIGNAL-051; own links per CLAUDE.md) has no owner in docs/06.
- **Restricted-content policy** (paging, cellular, 47 USC 605): C22, C24 or a cross-cutting layer? docs/06 names none.
- FEC decoding and deframing: a C22 stage or a library shared with C21?
- Per-decoder licence ledger; IMBE/AMBE vocoder licensing for dsd-fme.
- Start with 3–5 decoders; avoid general plugin infrastructure (one-developer constraint).

## Reading list
1. `docs/03 §3.3 "Automatic device and protocol decoders"`
2. `docs/03 §5.2 "Best UX ideas that already exist (steal these)"`
3. `docs/04 §7.5 "How existing tools approach it"`
4. `docs/03 §7 "Implications for This Project (short)"`
5. `docs/04 §1.3 "Legal considerations (US; not legal advice)"`
6. `docs/01 §3.3 "Apps, external apps (.ppma), and the catalog"`
