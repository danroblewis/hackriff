# ADR-0001 — Pipeline runtime and live reconfiguration

**Status:** PROVISIONAL (gated on spikes S1 live-reconfiguration and S2 sustained-throughput; see [docs/09](../09-risks-and-spikes.md))
**Touches:** C01–C11, C19–C24; the sample path; [ADR-0003](0003-process-plugin-model.md), [ADR-0007](0007-compute-placement.md), [ADR-0010](0010-language-and-licence-ledger.md)

## Context

The hard requirement (CLAUDE.md): change and extend signal-processing pipelines **without stopping capture or rebuilding**. The user's past GNU Radio experience was "lots of recompiling". We must say where that recompiling comes from and whether it is inherent.

**Where GNU Radio's recompiling/restarting comes from:**
1. **C++ out-of-tree (OOT) blocks** must be compiled and installed before use — a new block is a build step.
2. **GRC generates a Python program** from the flowgraph; changing what you process means editing the flowgraph and re-running it.
3. **Topology changes need a flowgraph restart.** GR 3.x supports `lock()`/`unlock()` to modify a running graph, but it is limited and fragile; adding N demodulators for N newly detected signals at runtime needs custom message-passing or a restart ([docs/03 §1.1](../03-sdr-software.md)).

So the recompile/restart pain is **inherent to GRC's static-flowgraph model, and avoidable** two ways: (a) a runtime whose graph is dynamically reconfigurable, or (b) an architecture where new processing chains are *data/plugins*, not compiled artifacts. We use both.

## Options

| Option | Live reconfiguration | Ecosystem | Licence | Fit |
|---|---|---|---|---|
| **GNU Radio 3.10** | Restart/limited lock-unlock; OOT blocks recompile | Largest block library, mature | GPLv3 | Fails the hard requirement as a core; would pull GPLv3 into the sample path |
| **GNU Radio 4** (RC1, Mar 2026) | **Runtime graph reconfiguration is a headline feature** — add/remove/reconnect blocks while running (verified: gnuradio.org news 2026-03-22; fair-acc/gnuradio4 RC1); SIMD, compile-time block merging, no thread-per-block, WASM target | Pre-ecosystem: no Python bindings/GUI yet, few blocks, governance churn ([docs/03 §1.2](../03-sdr-software.md)) | MIT core; GR3-ported blocks GPLv3 | Right architecture, wrong maturity for *today* |
| **FutureSDR** (Rust, v0.6–0.8) | Async runtime, dynamic; runs Linux/WASM/Android; custom buffers for Zynq/Vulkan; Burn ML hooks | Small community, few ready blocks ([docs/03 §1.5](../03-sdr-software.md)) | Apache-2.0/MIT (verify) | Strong fit if maturity holds |
| **Custom Rust dataflow on liquid-dsp + cuFFT (+ selective VOLK behind a process boundary)** | We own the scheduler; dynamic nodes by construction | Build it ourselves; liquid-dsp (MIT) gives filters/modems/framing | liquid-dsp MIT; core stays licence-flexible | Most control, most work |
| Hybrid | — | — | — | Chosen |

## Decision (provisional)

1. **Split the always-on core from the reconfigurable chains.** The core — source → RAM ring buffer → sweep/dwell → spectral estimation → noise floor → detection → channelizer — runs continuously and is not edited at runtime. Demod/decode **chains are added and removed at runtime** as (a) data-driven DDC+demod nodes inside the core, and (b) plugin processes ([ADR-0003](0003-process-plugin-model.md)). New chains never require recompiling or restarting capture. This satisfies the hard requirement independent of the DSP framework.
2. **Language: Rust for the core sample path and control plane** ([ADR-0010](0010-language-and-licence-ledger.md)); Python for orchestration/research/test tooling only (CLAUDE.md).
3. **DSP kernels: liquid-dsp (MIT) + cuFFT/CuPy on the Jetson GPU**, with VOLK/GNU Radio allowed only behind the plugin process boundary so their GPLv3 does not bind the core.
4. **Runtime engine: prefer FutureSDR** (Rust, dynamic, WASM, GPU/Burn hooks) as the dataflow substrate **if spike S1 confirms** runtime reconfiguration and throughput; otherwise fall back to a small owned Rust dataflow around liquid-dsp/cuFFT. **GR4 is tracked, borrowed from, and reachable via a plugin, but not the core dependency yet** — its ecosystem is 1–2 years behind and its governance is unsettled.
5. **GPU** does the wideband FFT, the polyphase channelizer, persistence, and ML inference ([ADR-0007](0007-compute-placement.md)).

## Consequences

- The recompile problem is designed out: chains are data and plugins, and the core runtime (FutureSDR or owned) supports dynamic graphs.
- We accept building or maturing a Rust DSP stack rather than inheriting GNU Radio's block library; we mitigate by wrapping existing decoders as plugins ([ADR-0003](0003-process-plugin-model.md)) rather than reimplementing them.
- Licence flexibility is preserved for the core (no GPL in the sample path). The project licence itself stays open ([ADR-0010](0010-language-and-licence-ledger.md)).
- Two spikes gate this: **S1** — demonstrate adding/removing a demod chain on a running capture in the chosen runtime; **S2** — sustain 20 Msps USB ingest + a GPU FFT within the Jetson power budget. If S1 fails for FutureSDR, the owned-dataflow fallback still meets the requirement; if it failed for *both*, we would reconsider GR4 despite its ecosystem gap.

## Alternatives explicitly rejected

- **GNU Radio 3.10 as the core** — the static-flowgraph/OOT-recompile model is exactly what the user wants to escape, and it forces GPLv3 into the sample path.
- **A pure-Python real-time path** — ruled out by CLAUDE.md (Python is orchestration/research only) and by throughput.
