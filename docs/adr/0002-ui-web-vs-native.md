# ADR-0002 — UI: web versus native

**Status:** PROVISIONAL (the user calls this one of the most important decisions; chosen to be reversible, and gated on spike S3 waterfall frame rate)
**Touches:** C39; the control plane; [ADR-0004](0004-stream-output-contract.md)

## Context

The device needs: an on-device display (small, touch, a few physical controls), remote viewing from a phone or laptop, a GPU waterfall that keeps up at 20 Msps, acceptable battery cost, and — with one developer — fast build velocity. The UI is exploratory (waterfall, persistence, signal table, per-signal inspector, history browser, attack-map), not a tune-and-listen panel.

Reference points from the research ([docs/03](../03-sdr-software.md)): **Maia SDR** serves a WASM/WebGL2 waterfall to a phone browser from a small Rust server on the device — the best embedded precedent. **SigDigger** (native Qt/OpenGL) has the inspector UX we want but is desktop-bound. **OpenDigitizer** (GR4 + Dear ImGui) compiles natively *and* to WASM. **SDR++** is native ImGui, fast, but tune-first. **GNU Radio World** compiles GR + Qt GUI to WASM in-browser.

## Options

| Option | On-device | Remote | Waterfall | Battery | 1-dev velocity |
|---|---|---|---|---|---|
| **Native (SDL/Dear ImGui)** | Lowest-latency GPU render | Needs a separate remote path (VNC/stream) | Best | Lowest | Slower to build rich exploratory UI + remote |
| **Web (WASM + WebGL2), headless core serves it** | Kiosk browser on the device screen | Same app in any phone/laptop browser, free | WebGL2 waterfall performs (Maia proves it) | Browser overhead, acceptable on Jetson | Fastest: one codebase, huge ecosystem |
| **Hybrid** (headless core + web default, optional native shell for the on-device waterfall only) | Native waterfall if the browser can't keep up | Web everywhere else | Best where it matters | Middle | Adds a second render path only if needed |

## Decision (provisional)

**Headless core with an API-first control plane, and a web UI (TypeScript + WASM, WebGL2 for the waterfall) served by the device.** The same app is the on-device kiosk browser and the remote phone/laptop client. This is the Maia SDR architecture generalised.

Rationale:
- **One codebase for on-device and remote.** With one developer this is the dominant factor; a native app plus a separate remote-viewing path is two UIs.
- **API-first matches the "scriptable from day one" lesson** (Mayhem's USB shell, SDRangel's REST). The UI is just the first client of the same API external programs use ([ADR-0004](0004-stream-output-contract.md)).
- **WebGL2 waterfalls are proven** at these rates (Maia SDR, WebFFT). GPU FFT/persistence happens in the core; the browser renders a texture.
- **Reversible.** Because the core is headless behind an API, swapping or adding a native shell later changes only the client, not the system.

**Fallback (the hybrid):** if spike S3 shows the browser waterfall can't hold an acceptable frame rate for the *on-device* screen at full persistence, add a thin native shell (Dear ImGui or SDL) that renders only the on-device waterfall against the same core API, keeping web for everything else and all remote use.

## Consequences

- The core must expose a clean streaming API early (spectrum frames, inventory, detections) — good, since external consumers need it anyway ([ADR-0004](0004-stream-output-contract.md)).
- Battery cost of a browser is real; measured in the power spike (S6). The web client can throttle frame rate in low-power mode.
- We defer choosing a web framework (plain TS + a WebGL2 canvas, or a light reactive layer) to implementation; it does not affect the architecture.
- The physical controls (encoder, buttons) map to API actions, so they work regardless of which client renders.

## Alternatives explicitly rejected

- **Native-only (SDR++/SigDigger style)** — best render, but ties the display to the device, needs a bolt-on remote path, and is the slowest for one developer to build the exploratory UI in.
- **Ship the flowgraph editor as the UI (GNU Radio World style)** — keeps the flowgraph-centric model this project is trying to get away from ([docs/03 §5.1](../03-sdr-software.md)).
