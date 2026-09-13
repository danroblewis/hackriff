# ADR-0009 — Hardware platform sketch

**Status:** PROVISIONAL (a sketch to set constraints, not a final BOM; the shopping list with timing is in [docs/12](../12-implementation-plan.md))
**Touches:** [ADR-0007](0007-compute-placement.md); C01, C05, C06, C32, C37

## Context

CLAUDE.md fixes the RF head (HackRF One, front end abstracted for HackRF Pro / other SDRs later) and compute (NVIDIA Jetson, Orin Nano Super class), form factor (portable handheld, cyberdeck-class acceptable), receive-first, low-power modes matter. This ADR sizes the rest so software can assume a target; it does not finalise hardware.

## Decision (provisional)

| Subsystem | Choice | Why / by when |
|---|---|---|
| **Compute** | **Jetson Orin Nano Super**, 8 GB, dev kit first ($249, verified 2026; JetPack 6.2, 67 TOPS, 7/15/25 W + MAXN, unified memory) | Confirmed current module; GPU for FFT/channelizer/ML ([ADR-0007](0007-compute-placement.md)). Dev kit for the bench; a module + carrier for the enclosure later. |
| **RF head** | **HackRF One** (USB 2.0, 20 Msps, 8-bit, 1 MHz–6 GHz), abstracted per [ADR-0001](0001-pipeline-runtime.md); HackRF Pro a drop-in upgrade | Given. Pro adds TCXO/NF/no-DC-spike later. |
| **Storage** | NVMe SSD (256 GB–1 TB) on the carrier's M.2 | The IQ pool + spectrum pyramid + state ([ADR-0006](0006-storage.md)). |
| **Display/input** | Web UI first on a **phone/laptop** (headless, [ADR-0002](0002-ui-web-vs-native.md)); add a **5–7″ touch IPS** + encoder + a few buttons for the standalone enclosure | Lets software start before the enclosure exists; the on-device screen is a later milestone. |
| **Clock** | TCXO in the HackRF path; **GPSDO (10 MHz into CLKIN)** as an accessory for timing/DF and marginal-HF science | No hardware 1PPS on HackRF One (docs/06 §5); GPSDO is how we get disciplined time. |
| **Front-end accessories (staged)** | 1) **FM/cellular notch + switched sub-octave preselector bank** (via Opera Cake frequency mode); 2) **LNA + bias-tee** (switchable); 3) **HF upconverter / active loop**; 4) **VLF E-field/coil + soundcard**; 5) **Ku LNB + dish**; 6) directional antenna (Yagi/LPDA) | The preselector/notch is the highest-value addition for trustworthy detection in a city ([docs/02 §1.6–1.7](../02-sdr-landscape.md)); the rest unlock the `needs-accessory` clusters (docs/06 §4.3) as their milestones arrive. |
| **Power** | Battery sized to the Tier-A envelope (~9–18 W, [docs/02 §7.2](../02-sdr-landscape.md)); target a runtime with a ~99–158 Wh pack; low-power survey mode extends it | Low-power modes matter (CLAUDE.md); thermal is spike S6. |
| **Enclosure** | Cyberdeck-class; passive/active cooling for the Jetson; not benchtop | PortaPack-sized is unlikely; cyberdeck acceptable (CLAUDE.md). |

## Consequences

- Software targets Orin Nano Super + HackRF One + NVMe from day one; the phone-as-display choice means UI and enclosure are decoupled and can lag.
- The front end is the biggest lever on detection quality; the preselector/notch bank is prioritised in the shopping list ([docs/12](../12-implementation-plan.md)) even though it is an accessory.
- Thermal and battery are open risks with spikes (S6); the design assumes the low-power mode is the default idle state.
- Prices are current-2026 but **verify at purchase** ([docs/12](../12-implementation-plan.md) marks each).
