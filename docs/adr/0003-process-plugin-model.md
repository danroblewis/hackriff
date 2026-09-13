# ADR-0003 — Process and plugin model

**Status:** PROVISIONAL
**Touches:** C15, C20, C22, C23, C36; [ADR-0001](0001-pipeline-runtime.md), [ADR-0004](0004-stream-output-contract.md), [ADR-0010](0010-language-and-licence-ledger.md)

## Context

No open tool covers the whole chain, but proven decoders exist for most known protocols ([docs/03 §3](../03-sdr-software.md)): rtl_433, dump1090/readsb, dump978, multimon-ng, AIS-catcher, dsd-fme, acarsdec/dumpvdl2, radiosonde_auto_rx, SatDump, gr-satellites, GNSS-SDR. We must reuse, not rewrite. Three forces: crash isolation (a decoder segfault must not drop capture), licence isolation (many are GPLv3; the core is licence-flexible per [ADR-0010](0010-language-and-licence-ledger.md)), and the "add a chain without recompiling" requirement ([ADR-0001](0001-pipeline-runtime.md)). The team is one developer plus a friend — do **not** over-build plugin infrastructure for hypothetical contributors (CLAUDE.md).

## Options

- **In-process only** (link every decoder into the core): fastest, but any crash kills capture and GPLv3 tools bind the core's licence. Rejected for third-party tools.
- **Subprocess/IPC for all decoders**: crash- and licence-isolated; new decoders are launched, not compiled in; small per-message overhead. The IQEngine plugin model (IQ/audio/bits in → messages/annotations out) generalised to live streams.
- **WASM/sandbox plugins**: portable and isolated, but re-wrapping mature C decoders into WASM is work with little payoff now.

## Decision (provisional)

- **Two plugin classes:**
  - **In-process (Rust) modules** for demodulators and estimators we own (C13/C14/C19/C20). They run in the core for latency and share the channelizer output directly.
  - **Subprocess plugins** for existing external tools and for anything GPLv3 (C22, C23 vocoders, C36 GNSS-SDR, and heavy ML if it needs its own runtime). One process per decoder instance, spoken to over the data-plane IPC ([ADR-0004](0004-stream-output-contract.md)).
- **Plugin manifest** (declarative): input type (IQ / channel / audio / bits), expected centre/bandwidth/sample-rate, parameters, output message schema, and licence. The scheduler and router use it to feed the right channelizer output to the right plugin.
- **Routing** is driven by classification and priors: a detected+classified emission (C15/C17) selects a pipeline from the registry (the SatDump "signal type → chain → products" pattern, [docs/03 §3.6](../03-sdr-software.md)).
- **Restricted-content and own-key policy** ride the plugin boundary: a plugin declares its output `content_class`; gating is enforced at stream-output ([ADR-0004](0004-stream-output-contract.md)); own-key decryption is a plugin fed user keys with key-source provenance (docs/06 §5, provisional).
- **Keep it small.** A handful of manifests and one IPC contract, not a plugin marketplace.

## Consequences

- A decoder crash restarts one subprocess; capture is untouched.
- GPLv3 decoders stay at arm's length; the core licence is unconstrained by them ([ADR-0010](0010-language-and-licence-ledger.md)).
- New protocol support = ship a manifest + wrap a binary, no core rebuild — satisfies the live-reconfiguration requirement for the decode layer.
- Cost: per-message IPC overhead and process management. Acceptable at event rates; the sample path never crosses the boundary.
